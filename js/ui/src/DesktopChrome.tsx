import {
  For,
  Show,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
  type JSX,
} from "solid-js";
import { Portal } from "solid-js/web";
import {
  MENU_NODE_CHECKMARK,
  MENU_NODE_ENABLED,
  MENU_NODE_RADIO,
  MENU_NODE_SEPARATOR,
  MENU_NODE_SUBMENU,
  MENU_NODE_VISIBLE,
  MPRIS_CAN_CONTROL,
  MPRIS_CAN_GO_NEXT,
  MPRIS_CAN_GO_PREVIOUS,
  MPRIS_CAN_PAUSE,
  MPRIS_CAN_PLAY,
  MPRIS_CAN_SEEK,
  TRAY_MENU_OK,
  TRAY_STATUS_NEEDS_ATTENTION,
  TRAY_STATUS_PASSIVE,
  type BlitConnectionSnapshot,
  type BlitWorkspace,
  type DesktopImage,
  type DesktopNotification,
  type MprisAction,
  type MprisArtwork,
  type MprisPlayer,
  type PortalRequest,
  type PortalChoiceValue,
  type TrayItem,
  type TrayMenu,
  type TrayMenuNode,
} from "@blit-sh/core";
import { createRemoteCommandAnchor } from "./mediaSessionAnchor";
import { desktopWorkerRegistration } from "./preview";
import {
  desktopDelivery,
  desktopNativeTag,
  canRaiseMpris,
  desktopNotificationHasDetail,
  matchesDesktopNotification,
  mprisMediaSessionKey,
  portalDialogFocusTarget,
  reconcileMprisSubscriptions,
  samePortalPresentationEntry,
  selectMediaSessionEntry,
  trayPrimaryOpensMenu,
  type MprisSubscriptionTarget,
} from "./desktopPresentation";
import { t, tp } from "./i18n";
import { mergeStyle, ui, z, type Theme, type UIScale } from "./theme";

type TrayEntry = {
  connectionId: string;
  connectionLabel: string;
  readOnly: boolean;
  item: TrayItem;
};

type NotificationEntry = {
  connectionId: string;
  connectionLabel: string;
  bootGeneration: bigint | null;
  readOnly: boolean;
  item: DesktopNotification;
};

type Toast = NotificationEntry & { key: string };
type MenuState = { entry: TrayEntry; menu: TrayMenu };
type MprisEntry = {
  connectionId: string;
  connectionLabel: string;
  readOnly: boolean;
  player: MprisPlayer;
};
type PortalEntry = {
  connectionId: string;
  connectionLabel: string;
  readOnly: boolean;
  request: PortalRequest;
};

function imageUrl(
  image: DesktopImage | { png: Uint8Array },
): string | undefined {
  if (image.png.length === 0) return undefined;
  let binary = "";
  for (let offset = 0; offset < image.png.length; offset += 0x8000) {
    binary += String.fromCharCode(
      ...image.png.subarray(offset, offset + 0x8000),
    );
  }
  return `data:image/png;base64,${btoa(binary)}`;
}

/**
 * Source for a player's cover. A forwarded URL is used as-is so the browser
 * fetches it off this thread and caches it across track changes; only art that
 * arrived as bytes pays the base64 encode below.
 */
function artworkUrl(artwork: MprisArtwork | null): string | undefined {
  if (!artwork) return undefined;
  return artwork.kind === "url" ? artwork.url : imageUrl(artwork);
}

function mediaTime(microseconds: number): string {
  if (!Number.isFinite(microseconds) || microseconds < 0) return "--:--";
  const seconds = Math.floor(microseconds / 1_000_000);
  const minutes = Math.floor(seconds / 60);
  return `${minutes}:${String(seconds % 60).padStart(2, "0")}`;
}

function notificationKey(connectionId: string, id: number): string {
  return `${connectionId}:${id}`;
}

async function postWorker(message: object): Promise<void> {
  const registration = await desktopWorkerRegistration();
  registration?.active?.postMessage(message);
}

function notificationTitle(item: DesktopNotification): string {
  return item.summary || item.appName || t("desktop.notification");
}

function Popup(props: {
  theme: Theme;
  scale: UIScale;
  children: JSX.Element;
  width?: string;
  maxHeight?: string;
}) {
  return (
    <div
      style={{
        position: "absolute",
        bottom: "100%",
        right: 0,
        "margin-bottom": `${props.scale.tightGap}px`,
        width: props.width ?? "min(28em, calc(100vw - 2em))",
        "max-height": props.maxHeight ?? "min(70vh, 36em)",
        overflow: "auto",
        "background-color": props.theme.solidPanelBg,
        color: props.theme.fg,
        border: `1px solid ${props.theme.border}`,
        "box-shadow": "0 8px 24px rgba(0,0,0,0.35)",
        "z-index": z.statusMenu,
      }}
    >
      {props.children}
    </div>
  );
}

function MprisChrome(props: {
  workspace: BlitWorkspace;
  connections: readonly BlitConnectionSnapshot[];
  connectionLabels: ReadonlyMap<string, string>;
  readOnlyConnections: ReadonlySet<string>;
  theme: Theme;
  scale: UIScale;
  compact: boolean;
  focusedConnectionId?: string;
  closeOthers: () => void;
}) {
  const [open, setOpen] = createSignal(false);
  const [manualMediaSessionKey, setManualMediaSessionKey] =
    createSignal<string>();
  const [playingOrderRevision, setPlayingOrderRevision] = createSignal(0);
  const playingStates = new Map<string, boolean>();
  const playingOrder = new Map<string, number>();
  let playingClock = 0;
  const players = createMemo<MprisEntry[]>(() => {
    const entries: MprisEntry[] = [];
    for (const snapshot of props.connections) {
      if (!snapshot.supportsDesktopMedia) continue;
      const connection = props.workspace.getConnection(snapshot.id);
      if (!connection) continue;
      for (const player of connection.mprisStore.players.values()) {
        entries.push({
          connectionId: snapshot.id,
          connectionLabel: props.connectionLabels.get(snapshot.id) ?? "",
          readOnly: props.readOnlyConnections.has(snapshot.id),
          player,
        });
      }
    }
    return entries.sort(
      (a, b) =>
        Number(b.player.active) - Number(a.player.active) ||
        props.connections.findIndex((item) => item.id === a.connectionId) -
          props.connections.findIndex((item) => item.id === b.connectionId) ||
        a.player.playerId - b.player.playerId,
    );
  });
  createEffect(() => {
    const live = new Set<string>();
    let changed = false;
    for (const entry of players()) {
      const key = mprisMediaSessionKey(entry);
      live.add(key);
      const playing =
        !entry.readOnly &&
        entry.player.active &&
        entry.player.playbackStatus === "playing";
      if (playing && playingStates.get(key) !== true) {
        playingOrder.set(key, ++playingClock);
        if (
          manualMediaSessionKey() !== undefined &&
          manualMediaSessionKey() !== key
        ) {
          setManualMediaSessionKey(undefined);
        }
        changed = true;
      }
      playingStates.set(key, playing);
    }
    for (const key of [...playingStates.keys()]) {
      if (live.has(key)) continue;
      playingStates.delete(key);
      playingOrder.delete(key);
      changed = true;
    }
    if (changed) setPlayingOrderRevision((revision) => revision + 1);
  });
  const mediaSessionActive = createMemo(() => {
    playingOrderRevision();
    return selectMediaSessionEntry(
      players(),
      props.focusedConnectionId,
      playingOrder,
      manualMediaSessionKey(),
    );
  });
  const active = createMemo(() => {
    const coordinated = mediaSessionActive();
    return (
      (coordinated?.player.active ? coordinated : undefined) ??
      players().find((entry) => entry.player.active) ??
      coordinated ??
      players()[0]
    );
  });

  const mprisSubscriptions = new Set<MprisSubscriptionTarget>();
  createEffect(() => {
    const stores = props.connections
      .filter((snapshot) => snapshot.supportsDesktopMedia)
      .map((snapshot) => props.workspace.getConnection(snapshot.id)?.mprisStore)
      .filter((store) => store !== undefined);
    reconcileMprisSubscriptions(mprisSubscriptions, stores);
  });
  onCleanup(() => reconcileMprisSubscriptions(mprisSubscriptions, []));

  const act = (entry: MprisEntry, action: MprisAction) => {
    if (entry.readOnly) return;
    const pending = props.workspace
      .getConnection(entry.connectionId)
      ?.mprisStore.act(entry.player.playerId, action);
    if (!pending) return;
    void pending
      .then(() => {
        if (action.kind === "select") {
          setManualMediaSessionKey(mprisMediaSessionKey(entry));
        }
      })
      .catch(() => undefined);
  };
  const capable = (entry: MprisEntry, flag: number) =>
    !entry.readOnly &&
    (entry.player.capabilityFlags & (MPRIS_CAN_CONTROL | flag)) ===
      (MPRIS_CAN_CONTROL | flag);

  // WebKit picks Now Playing artwork out of the array by `sizes` and shows
  // nothing when no entry declares one, so a cover without it reaches iPadOS as
  // a blank tile. Neither artwork kind carries dimensions — a forwarded URL has
  // none to send — so they are measured here instead: the browser has to decode
  // the image anyway, and its intrinsic size is the truth a server guess would
  // only approximate. `null` records a source that failed, so a broken cover is
  // attempted once rather than on every metadata change.
  const [artworkSizes, setArtworkSizes] = createSignal<
    ReadonlyMap<string, string | null>
  >(new Map());
  const measureArtwork = (src: string) => {
    if (artworkSizes().has(src)) return;
    setArtworkSizes((known) => new Map(known).set(src, null));
    const image = new Image();
    const settle = (size: string | null) =>
      setArtworkSizes((known) => {
        // One entry per track, and a track change makes the old one dead
        // weight; the cap keeps a long listening session from accumulating.
        const next =
          known.size >= 32 ? new Map<string, string | null>() : new Map(known);
        return next.set(src, size);
      });
    image.onload = () =>
      settle(
        image.naturalWidth > 0
          ? `${image.naturalWidth}x${image.naturalHeight}`
          : null,
      );
    image.onerror = () => settle(null);
    image.src = src;
  };

  // Created once and reused: the element itself is the routing target, so
  // rebuilding it per track would drop the audio session it exists to hold.
  const commandAnchor = createRemoteCommandAnchor();
  onCleanup(() => commandAnchor?.dispose());

  createEffect(() => {
    const entry = mediaSessionActive();
    if (!("mediaSession" in navigator)) return;
    const session = navigator.mediaSession;
    const actions: MediaSessionAction[] = [
      "play",
      "pause",
      "stop",
      "previoustrack",
      "nexttrack",
      "seekbackward",
      "seekforward",
      "seekto",
    ];
    const clear = () => {
      for (const action of actions) {
        try {
          session.setActionHandler(action, null);
        } catch {
          // A browser may expose Media Session without every action.
        }
      }
      session.metadata = null;
      session.playbackState = "none";
      try {
        session.setPositionState();
      } catch {
        // A partial implementation may expose Media Session without position.
      }
    };
    clear();
    if (!entry) {
      commandAnchor?.release();
      return;
    }
    const player = entry.player;
    const artwork = artworkUrl(player.artwork);
    if (artwork) measureArtwork(artwork);
    const size = artwork ? artworkSizes().get(artwork) : undefined;
    // Constructing metadata must not be able to cost the transport controls:
    // a throw here would skip the playback state and every action handler
    // below, leaving a Now Playing panel whose buttons do nothing.
    try {
      session.metadata = new MediaMetadata({
        title: player.title || player.identity,
        artist: player.artists.join(", "),
        album: player.album,
        artwork: artwork
          ? [size ? { src: artwork, sizes: size } : { src: artwork }]
          : [],
      });
    } catch {
      // A partial implementation may reject metadata it cannot represent.
    }
    session.playbackState =
      player.playbackStatus === "stopped" ? "none" : player.playbackStatus;
    const handler = (
      action: MediaSessionAction,
      enabled: boolean,
      callback: (details: MediaSessionActionDetails) => void,
    ) => {
      if (!enabled) return;
      try {
        session.setActionHandler(action, callback);
      } catch {
        // Unsupported action in a partially implemented browser API.
      }
    };
    handler("play", capable(entry, MPRIS_CAN_PLAY), () =>
      act(entry, { kind: "play" }),
    );
    handler("pause", capable(entry, MPRIS_CAN_PAUSE), () =>
      act(entry, { kind: "pause" }),
    );
    handler(
      "stop",
      !entry.readOnly && Boolean(player.capabilityFlags & MPRIS_CAN_CONTROL),
      () => act(entry, { kind: "stop" }),
    );
    handler("previoustrack", capable(entry, MPRIS_CAN_GO_PREVIOUS), () =>
      act(entry, { kind: "previous" }),
    );
    handler("nexttrack", capable(entry, MPRIS_CAN_GO_NEXT), () =>
      act(entry, { kind: "next" }),
    );
    handler("seekbackward", capable(entry, MPRIS_CAN_SEEK), (details) =>
      act(entry, {
        kind: "seek",
        offsetUs: -Math.round((details.seekOffset ?? 10) * 1_000_000),
      }),
    );
    handler("seekforward", capable(entry, MPRIS_CAN_SEEK), (details) =>
      act(entry, {
        kind: "seek",
        offsetUs: Math.round((details.seekOffset ?? 10) * 1_000_000),
      }),
    );
    handler("seekto", capable(entry, MPRIS_CAN_SEEK), (details) => {
      if (details.seekTime === undefined) return;
      act(entry, {
        kind: "setPosition",
        positionUs: Math.round(details.seekTime * 1_000_000),
        trackRevision: player.trackRevision,
      });
    });
    // Hold the audio session only while something is actually controllable:
    // a player exposing no transport has no commands to route, and the session
    // is not worth claiming for a panel that would ignore it anyway.
    if (
      capable(entry, MPRIS_CAN_PLAY) ||
      capable(entry, MPRIS_CAN_PAUSE) ||
      capable(entry, MPRIS_CAN_GO_NEXT) ||
      capable(entry, MPRIS_CAN_GO_PREVIOUS)
    ) {
      commandAnchor?.engage();
    } else {
      commandAnchor?.release();
    }
    if (player.lengthUs > 0 && player.rate > 0) {
      try {
        const position =
          props.workspace
            .getConnection(entry.connectionId)
            ?.mprisStore.positionUs(player.playerId) ?? player.positionUs;
        session.setPositionState({
          duration: player.lengthUs / 1_000_000,
          playbackRate: player.rate,
          position: Math.min(position, player.lengthUs) / 1_000_000,
        });
      } catch {
        // Invalid or browser-rejected position state is non-fatal.
      }
    }
    onCleanup(clear);
  });

  const controls = (entry: MprisEntry) => (
    <span style={{ display: "flex", "align-items": "center" }}>
      <button
        disabled={!capable(entry, MPRIS_CAN_GO_PREVIOUS)}
        onClick={() => act(entry, { kind: "previous" })}
        title={t("desktop.mediaPrevious")}
        aria-label={t("desktop.mediaPrevious")}
        style={ui.btn}
      >
        ◀|
      </button>
      <button
        disabled={
          entry.player.playbackStatus === "playing"
            ? !capable(entry, MPRIS_CAN_PAUSE)
            : !capable(entry, MPRIS_CAN_PLAY)
        }
        onClick={() =>
          act(entry, {
            kind: entry.player.playbackStatus === "playing" ? "pause" : "play",
          })
        }
        title={
          entry.player.playbackStatus === "playing"
            ? t("desktop.mediaPause")
            : t("desktop.mediaPlay")
        }
        style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
      >
        {entry.player.playbackStatus === "playing" ? "Ⅱ" : "▶"}
      </button>
      <button
        disabled={!capable(entry, MPRIS_CAN_GO_NEXT)}
        onClick={() => act(entry, { kind: "next" })}
        title={t("desktop.mediaNext")}
        aria-label={t("desktop.mediaNext")}
        style={ui.btn}
      >
        |▶
      </button>
    </span>
  );

  return (
    <Show when={active()} keyed>
      {(current) => (
        <span style={{ display: "flex", "align-items": "center" }}>
          <Show when={!props.compact}>{controls(current)}</Show>
          <button
            onClick={() => {
              props.closeOthers();
              setOpen((value) => !value);
            }}
            title={current.player.title || current.player.identity}
            aria-label={t("desktop.mediaPlayers")}
            aria-haspopup="menu"
            aria-expanded={open()}
            style={{
              ...ui.btn,
              "max-width": props.compact ? "8em" : "14em",
              overflow: "hidden",
              "text-overflow": "ellipsis",
              "white-space": "nowrap",
              "font-size": `${props.scale.sm}px`,
            }}
          >
            {current.player.title || current.player.identity}
          </button>
          <Show when={open()}>
            <Popup
              theme={props.theme}
              scale={props.scale}
              width="min(30em, calc(100vw - 2em))"
            >
              <For each={players()}>
                {(entry) => {
                  const art = () => artworkUrl(entry.player.artwork);
                  return (
                    <article
                      style={{
                        display: "grid",
                        "grid-template-columns": "3em minmax(0, 1fr) auto",
                        gap: `${props.scale.gap}px`,
                        padding: `${props.scale.panelPadding}px`,
                        "border-bottom": `1px solid ${props.theme.subtleBorder}`,
                      }}
                    >
                      <Show
                        when={art()}
                        fallback={
                          <span
                            style={{
                              "font-size": "2em",
                              "text-align": "center",
                            }}
                          >
                            ♪
                          </span>
                        }
                      >
                        {(src) => (
                          <img
                            src={src()}
                            alt=""
                            width={48}
                            height={48}
                            style={{ "object-fit": "cover" }}
                          />
                        )}
                      </Show>
                      <button
                        disabled={entry.readOnly}
                        onClick={() => act(entry, { kind: "select" })}
                        style={{
                          ...ui.btn,
                          "min-width": 0,
                          "text-align": "left",
                        }}
                      >
                        <strong
                          style={{
                            display: "block",
                            overflow: "hidden",
                            "text-overflow": "ellipsis",
                          }}
                        >
                          {entry.player.title || entry.player.identity}
                        </strong>
                        <small style={{ color: props.theme.dimFg }}>
                          {[
                            entry.player.artists.join(", "),
                            entry.player.album,
                            entry.connectionLabel,
                          ]
                            .filter(Boolean)
                            .join(" · ")}
                        </small>
                        <Show when={entry.player.lengthUs >= 0}>
                          <small
                            style={{
                              display: "block",
                              color: props.theme.dimFg,
                            }}
                          >
                            {mediaTime(
                              props.workspace
                                .getConnection(entry.connectionId)
                                ?.mprisStore.positionUs(
                                  entry.player.playerId,
                                ) ?? entry.player.positionUs,
                            )}{" "}
                            / {mediaTime(entry.player.lengthUs)}
                          </small>
                        </Show>
                      </button>
                      <span
                        style={{
                          display: "flex",
                          "flex-direction": "column",
                          "align-items": "end",
                        }}
                      >
                        {controls(entry)}
                        <Show
                          when={canRaiseMpris(
                            entry.readOnly,
                            entry.player.capabilityFlags,
                          )}
                        >
                          <button
                            onClick={() => act(entry, { kind: "raise" })}
                            style={ui.btn}
                          >
                            {t("desktop.mediaRaise")}
                          </button>
                        </Show>
                      </span>
                    </article>
                  );
                }}
              </For>
            </Popup>
          </Show>
        </span>
      )}
    </Show>
  );
}

function PortalChrome(props: {
  workspace: BlitWorkspace;
  connections: readonly BlitConnectionSnapshot[];
  connectionLabels: ReadonlyMap<string, string>;
  readOnlyConnections: ReadonlySet<string>;
  theme: Theme;
  scale: UIScale;
}) {
  const [selected, setSelected] = createSignal<ReadonlySet<number>>(new Set());
  const [choiceValues, setChoiceValues] = createSignal<
    ReadonlyMap<string, string>
  >(new Map());
  let dialog: HTMLDivElement | undefined;
  let restoreFocus: Element | null = null;
  const requests = createMemo<PortalEntry[]>(() => {
    const entries: PortalEntry[] = [];
    for (const snapshot of props.connections) {
      const connection = props.workspace.getConnection(snapshot.id);
      if (!snapshot.supportsDesktopMedia || !connection) continue;
      if (props.readOnlyConnections.has(snapshot.id)) continue;
      for (const request of connection.mediaStore.requests.values()) {
        entries.push({
          connectionId: snapshot.id,
          connectionLabel:
            props.connectionLabels.get(snapshot.id) ?? snapshot.id,
          readOnly: props.readOnlyConnections.has(snapshot.id),
          request,
        });
      }
    }
    return entries;
  });
  const active = createMemo<PortalEntry | undefined>(
    () => requests()[0],
    undefined,
    { equals: samePortalPresentationEntry },
  );

  createEffect(() => {
    const entry = active();
    if (!entry) return;
    setSelected(new Set<number>());
    setChoiceValues(
      new Map(
        entry.request.kind === "access"
          ? entry.request.choices.map((choice) => [
              choice.id,
              choice.initialValue,
            ])
          : [],
      ),
    );
    restoreFocus = document.activeElement;
    queueMicrotask(() => dialog?.focus());
    onCleanup(() => {
      if (restoreFocus instanceof HTMLElement) restoreFocus.focus();
    });
  });

  const reply = (entry: PortalEntry, decision: "deny" | "grant") => {
    if (entry.readOnly) return;
    const choices: PortalChoiceValue[] = [...choiceValues()].map(
      ([id, value]) => ({
        id,
        value,
      }),
    );
    props.workspace
      .getConnection(entry.connectionId)
      ?.mediaStore.reply(
        entry.request.requestId,
        decision,
        entry.request.kind === "screencast" ? [...selected()] : [],
        entry.request.kind === "access" ? choices : [],
      );
  };

  return (
    <Show when={active()} keyed>
      {(entry) => (
        <Portal>
          <div
            style={{
              position: "fixed",
              inset: 0,
              display: "grid",
              "place-items": "center",
              padding: "1em",
              "background-color": "rgba(0,0,0,0.55)",
              "z-index": z.disconnected + 1,
            }}
          >
            <div
              ref={dialog}
              role="dialog"
              aria-modal="true"
              aria-labelledby="blit-portal-title"
              tabIndex={-1}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  event.preventDefault();
                  reply(entry, "deny");
                  return;
                }
                if (event.key !== "Tab" || !dialog) return;
                const target = portalDialogFocusTarget(
                  dialog,
                  document.activeElement,
                  event.shiftKey,
                );
                if (target) {
                  event.preventDefault();
                  target.focus();
                }
              }}
              style={{
                width: "min(42em, 100%)",
                "max-height": "min(80vh, 48em)",
                overflow: "auto",
                padding: `${props.scale.panelPadding}px`,
                "background-color": props.theme.solidPanelBg,
                color: props.theme.fg,
                border: `1px solid ${props.theme.border}`,
                "box-shadow": "0 12px 36px rgba(0,0,0,0.45)",
              }}
            >
              <h2 id="blit-portal-title" style={{ margin: 0 }}>
                {entry.request.kind === "access"
                  ? entry.request.title || t("desktop.portalAccess")
                  : t("desktop.portalScreenCast")}
              </h2>
              <p style={{ color: props.theme.dimFg }}>
                {[entry.request.appId, entry.connectionLabel]
                  .filter(Boolean)
                  .join(" · ")}
              </p>
              <Show
                when={entry.request.kind === "access" ? entry.request : null}
              >
                {(request) => (
                  <>
                    <Show when={request().subtitle}>
                      <h3>{request().subtitle}</h3>
                    </Show>
                    <p style={{ "white-space": "pre-wrap" }}>
                      {request().body}
                    </p>
                    <For each={request().choices}>
                      {(choice) => (
                        <label
                          style={{
                            display: "block",
                            "margin-top": `${props.scale.gap}px`,
                          }}
                        >
                          {choice.label}
                          <select
                            value={
                              choiceValues().get(choice.id) ??
                              choice.initialValue
                            }
                            onChange={(event) => {
                              const next = new Map(choiceValues());
                              next.set(choice.id, event.currentTarget.value);
                              setChoiceValues(next);
                            }}
                            style={{ display: "block", width: "100%" }}
                          >
                            <For each={choice.options}>
                              {(option) => (
                                <option value={option.id}>
                                  {option.value}
                                </option>
                              )}
                            </For>
                          </select>
                        </label>
                      )}
                    </For>
                  </>
                )}
              </Show>
              <Show
                when={
                  entry.request.kind === "screencast" ? entry.request : null
                }
              >
                {(request) => (
                  <fieldset style={{ border: 0, padding: 0 }}>
                    <legend>{t("desktop.portalChooseWindows")}</legend>
                    <div
                      style={{
                        display: "grid",
                        "grid-template-columns":
                          "repeat(auto-fit, minmax(12em, 1fr))",
                        gap: `${props.scale.gap}px`,
                      }}
                    >
                      <For each={request().candidates}>
                        {(candidate) => (
                          <label
                            style={{
                              display: "block",
                              padding: `${props.scale.tightGap}px`,
                              border: `1px solid ${
                                selected().has(candidate.surfaceId)
                                  ? props.theme.accent
                                  : props.theme.border
                              }`,
                            }}
                          >
                            <Show
                              when={imageUrl({ png: candidate.thumbnailPng })}
                            >
                              {(src) => (
                                <img
                                  src={src()}
                                  alt=""
                                  style={{
                                    width: "100%",
                                    "aspect-ratio": "16 / 9",
                                    "object-fit": "contain",
                                  }}
                                />
                              )}
                            </Show>
                            <input
                              type={request().multiple ? "checkbox" : "radio"}
                              name="blit-screencast-source"
                              checked={selected().has(candidate.surfaceId)}
                              onChange={() => {
                                const next = request().multiple
                                  ? new Set(selected())
                                  : new Set<number>();
                                if (next.has(candidate.surfaceId))
                                  next.delete(candidate.surfaceId);
                                else if (next.size < 4)
                                  next.add(candidate.surfaceId);
                                setSelected(next);
                              }}
                            />{" "}
                            <strong>
                              {candidate.title || candidate.appId}
                            </strong>
                            <small
                              style={{
                                display: "block",
                                color: props.theme.dimFg,
                              }}
                            >
                              {candidate.appId} · {candidate.width}×
                              {candidate.height}
                            </small>
                          </label>
                        )}
                      </For>
                    </div>
                  </fieldset>
                )}
              </Show>
              <div
                style={{
                  display: "flex",
                  "justify-content": "end",
                  gap: `${props.scale.gap}px`,
                  "margin-top": `${props.scale.panelPadding}px`,
                }}
              >
                <button
                  disabled={entry.readOnly}
                  onClick={() => reply(entry, "deny")}
                  style={ui.btn}
                >
                  {entry.request.kind === "access"
                    ? entry.request.denyLabel
                    : t("desktop.portalDeny")}
                </button>
                <button
                  disabled={
                    entry.readOnly ||
                    (entry.request.kind === "screencast" &&
                      selected().size === 0)
                  }
                  onClick={() => reply(entry, "grant")}
                  style={{
                    ...ui.btn,
                    padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
                    "background-color": props.theme.accent,
                  }}
                >
                  {entry.request.kind === "access"
                    ? entry.request.grantLabel
                    : t("desktop.portalShare")}
                </button>
              </div>
            </div>
          </div>
        </Portal>
      )}
    </Show>
  );
}

/** Summary, a dim line of provenance, then whatever detail the sender supplied.
 *  Nothing is hidden: the content image is clamped to a thumbnail rather than
 *  banner-sized, which is what used to make a single notification take over the
 *  popup. Clicking the row keeps its freedesktop meaning and activates the
 *  default action, so that action needs no button of its own. */
function NotificationCard(props: {
  entry: NotificationEntry;
  theme: Theme;
  scale: UIScale;
  toast?: boolean;
  invoke: (key: string | null) => void;
  dismiss: () => void;
}) {
  const icon = createMemo(() => imageUrl(props.entry.item.icon));
  const image = createMemo(() => imageUrl(props.entry.item.image));
  const detail = createMemo(() =>
    desktopNotificationHasDetail(props.entry.item),
  );
  const defaultAction = createMemo(() =>
    props.entry.item.actions.some((action) => action.key === "default"),
  );
  const extraActions = createMemo(() =>
    props.entry.item.actions.filter((action) => action.key !== "default"),
  );
  const provenance = () =>
    [props.entry.item.appName, props.entry.connectionLabel]
      .filter(Boolean)
      .join(" · ");
  const iconSize = () => Math.round(props.scale.icon / 2);
  return (
    <article
      style={{
        display: "grid",
        "grid-template-columns": "auto minmax(0, 1fr) auto",
        "align-items": "center",
        "column-gap": `${props.scale.gap}px`,
        "row-gap": `${props.scale.tightGap}px`,
        padding: `${props.scale.tightGap}px ${props.scale.gap}px`,
        "border-bottom": props.toast
          ? undefined
          : `1px solid ${props.theme.subtleBorder}`,
        "background-color": props.theme.solidPanelBg,
        color: props.theme.fg,
        "font-size": `${props.scale.md}px`,
      }}
    >
      {/* The column is reserved even without an icon: rows sit in a list, and
          a sender that ships no icon must not shift its neighbours' text. */}
      <Show
        when={icon()}
        fallback={<span style={{ width: `${iconSize()}px` }} />}
      >
        {(src) => (
          <img
            src={src()}
            alt=""
            width={iconSize()}
            height={iconSize()}
            style={{ "object-fit": "contain", "align-self": "start" }}
          />
        )}
      </Show>
      <button
        disabled={!defaultAction() || props.entry.readOnly}
        onClick={() => props.invoke(null)}
        style={mergeStyle(ui.btn, {
          display: "block",
          "min-width": 0,
          padding: 0,
          opacity: 1,
          "font-size": "inherit",
          "text-align": "left",
          cursor:
            defaultAction() && !props.entry.readOnly ? "pointer" : "default",
        })}
      >
        <strong style={{ display: "block", "overflow-wrap": "anywhere" }}>
          {notificationTitle(props.entry.item)}
        </strong>
        <Show when={provenance()}>
          <small
            style={{
              display: "block",
              color: props.theme.dimFg,
              "font-size": `${props.scale.sm}px`,
              "overflow-wrap": "anywhere",
            }}
          >
            {provenance()}
          </small>
        </Show>
      </button>
      <button
        disabled={props.entry.readOnly}
        onClick={props.dismiss}
        title={t("desktop.dismiss")}
        aria-label={t("desktop.dismiss")}
        style={mergeStyle(ui.btn, {
          "align-self": "start",
          color: props.theme.dimFg,
          "font-size": `${props.scale.lg}px`,
          "line-height": 1,
          padding: `${props.scale.tightGap}px`,
        })}
      >
        ×
      </button>
      <Show when={detail()}>
        <div
          style={{
            "grid-column": 2,
            display: "grid",
            /* The sender's content image is a thumbnail beside the body, not a
               banner above it: senders ship 512px squares for a 16px slot. */
            "grid-template-columns": image()
              ? "minmax(0, 1fr) auto"
              : "minmax(0, 1fr)",
            "align-items": "start",
            gap: `${props.scale.tightGap}px`,
            "padding-bottom": `${props.scale.tightGap}px`,
          }}
        >
          <Show when={props.entry.item.body}>
            <span
              style={{
                "white-space": "pre-wrap",
                "overflow-wrap": "anywhere",
              }}
            >
              {props.entry.item.body}
            </span>
          </Show>
          <Show when={image()}>
            {(src) => (
              <img
                src={src()}
                alt=""
                style={{
                  "grid-column": 2,
                  "grid-row": 1,
                  "max-width": `${props.scale.icon}px`,
                  "max-height": `${props.scale.icon}px`,
                  "object-fit": "contain",
                }}
              />
            )}
          </Show>
          <Show when={extraActions().length > 0}>
            <div
              style={{
                "grid-column": "1 / -1",
                display: "flex",
                "flex-wrap": "wrap",
                gap: `${props.scale.tightGap}px`,
              }}
            >
              <For each={extraActions()}>
                {(action) => (
                  <button
                    disabled={props.entry.readOnly}
                    onClick={() => props.invoke(action.key)}
                    style={mergeStyle(ui.btn, {
                      "font-size": `${props.scale.sm}px`,
                      padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
                      border: `1px solid ${props.theme.border}`,
                    })}
                  >
                    {action.label}
                  </button>
                )}
              </For>
            </div>
          </Show>
        </div>
      </Show>
    </article>
  );
}

function MenuNodes(props: {
  nodes: readonly TrayMenuNode[];
  parentId: number;
  depth: number;
  readOnly: boolean;
  theme: Theme;
  scale: UIScale;
  openSubmenu: (id: number) => void;
  click: (id: number) => void;
}) {
  const children = createMemo(() =>
    props.nodes
      .filter(
        (node) =>
          node.parentId === props.parentId &&
          (node.flags & MENU_NODE_VISIBLE) !== 0,
      )
      .sort((a, b) => a.position - b.position),
  );
  return (
    <div role={props.depth === 0 ? "menu" : "group"}>
      <For each={children()}>
        {(node) => {
          const separator = () => (node.flags & MENU_NODE_SEPARATOR) !== 0;
          const submenu = () => (node.flags & MENU_NODE_SUBMENU) !== 0;
          const checked = () => node.toggleState === 1;
          const role = () =>
            node.flags & MENU_NODE_RADIO
              ? "menuitemradio"
              : node.flags & MENU_NODE_CHECKMARK
                ? "menuitemcheckbox"
                : "menuitem";
          return (
            <Show
              when={!separator()}
              fallback={
                <hr
                  role="separator"
                  style={{
                    border: 0,
                    "border-top": `1px solid ${props.theme.border}`,
                  }}
                />
              }
            >
              <button
                role={role()}
                aria-checked={
                  node.flags & (MENU_NODE_RADIO | MENU_NODE_CHECKMARK)
                    ? checked()
                    : undefined
                }
                aria-haspopup={submenu() ? "menu" : undefined}
                disabled={
                  props.readOnly || (node.flags & MENU_NODE_ENABLED) === 0
                }
                onClick={() =>
                  submenu() ? props.openSubmenu(node.id) : props.click(node.id)
                }
                style={{
                  ...ui.btn,
                  width: "100%",
                  display: "grid",
                  "grid-template-columns": "1.25em minmax(0, 1fr) auto",
                  gap: `${props.scale.tightGap}px`,
                  padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
                  "padding-left": `${props.scale.controlX + props.depth * props.scale.gap}px`,
                  "text-align": "left",
                  opacity:
                    (node.flags & MENU_NODE_ENABLED) === 0 || props.readOnly
                      ? 0.5
                      : 1,
                }}
              >
                <span>
                  <Show
                    when={imageUrl(node.icon)}
                    fallback={
                      node.toggleState >= 0 ? (checked() ? "✓" : "") : ""
                    }
                  >
                    {(src) => <img src={src()} alt="" width={16} height={16} />}
                  </Show>
                </span>
                <span>{node.label}</span>
                <span>{submenu() ? "›" : ""}</span>
              </button>
              <Show when={submenu()}>
                <MenuNodes
                  {...props}
                  parentId={node.id}
                  depth={props.depth + 1}
                />
              </Show>
            </Show>
          );
        }}
      </For>
    </div>
  );
}

export function DesktopChrome(props: {
  workspace: BlitWorkspace;
  connections: readonly BlitConnectionSnapshot[];
  connectionLabels: ReadonlyMap<string, string>;
  readOnlyConnections: ReadonlySet<string>;
  theme: Theme;
  scale: UIScale;
  compact: boolean;
  focusedConnectionId?: string;
}) {
  const [toasts, setToasts] = createSignal<Toast[]>([]);
  const [bellOpen, setBellOpen] = createSignal(false);
  const [trayOpen, setTrayOpen] = createSignal(false);
  const [menu, setMenu] = createSignal<MenuState | null>(null);
  const [permission, setPermission] = createSignal<NotificationPermission>(
    typeof Notification === "undefined" ? "denied" : Notification.permission,
  );
  const toastTimers = new Map<string, ReturnType<typeof setTimeout>>();
  const nativeShown = new Map<
    string,
    {
      tag: string;
      connectionId: string;
      bootGeneration: string;
      notificationId: number;
    }
  >();
  let root: HTMLSpanElement | undefined;

  const tray = createMemo<TrayEntry[]>(() => {
    const entries: TrayEntry[] = [];
    for (const snapshot of props.connections) {
      const connection = props.workspace.getConnection(snapshot.id);
      if (!snapshot.supportsDesktop || !connection) continue;
      for (const item of connection.desktopStore.tray.values()) {
        entries.push({
          connectionId: snapshot.id,
          connectionLabel: props.connectionLabels.get(snapshot.id) ?? "",
          readOnly: props.readOnlyConnections.has(snapshot.id),
          item,
        });
      }
    }
    return entries.sort(
      (a, b) =>
        props.connections.findIndex((item) => item.id === a.connectionId) -
          props.connections.findIndex((item) => item.id === b.connectionId) ||
        a.item.category - b.item.category ||
        a.item.trayId - b.item.trayId,
    );
  });
  const visibleTray = createMemo(() =>
    tray().filter((entry) => entry.item.status !== TRAY_STATUS_PASSIVE),
  );
  const overflowTray = createMemo(() => {
    const shown = new Set(
      visibleTray()
        .slice(0, props.compact ? 0 : 4)
        .map((entry) => `${entry.connectionId}:${entry.item.trayId}`),
    );
    return tray().filter(
      (entry) => !shown.has(`${entry.connectionId}:${entry.item.trayId}`),
    );
  });
  const desktopEnabled = createMemo(() =>
    props.connections.some((connection) => connection.supportsDesktop),
  );
  const notifications = createMemo<NotificationEntry[]>(() => {
    const entries: NotificationEntry[] = [];
    for (const snapshot of props.connections) {
      const connection = props.workspace.getConnection(snapshot.id);
      if (!snapshot.supportsDesktop || !connection) continue;
      for (const item of connection.desktopStore.notifications.values()) {
        entries.push({
          connectionId: snapshot.id,
          connectionLabel: props.connectionLabels.get(snapshot.id) ?? "",
          bootGeneration: snapshot.bootGeneration,
          readOnly: props.readOnlyConnections.has(snapshot.id),
          item,
        });
      }
    }
    return entries;
  });

  const invoke = (entry: NotificationEntry, key: string | null) => {
    if (entry.readOnly) return;
    const store = props.workspace.getConnection(
      entry.connectionId,
    )?.desktopStore;
    if (key == null) {
      store?.invokeDefault(entry.item.notificationId, entry.item.revision);
    } else {
      store?.invokeAction(entry.item.notificationId, entry.item.revision, key);
    }
  };
  const dismiss = (entry: NotificationEntry) => {
    if (entry.readOnly) return;
    props.workspace
      .getConnection(entry.connectionId)
      ?.desktopStore.dismiss(entry.item.notificationId, entry.item.revision);
  };

  const showNative = (entry: NotificationEntry) => {
    const tag = desktopNativeTag(
      entry.connectionId,
      entry.bootGeneration,
      entry.item.notificationId,
    );
    if (!tag) return;
    nativeShown.set(
      notificationKey(entry.connectionId, entry.item.notificationId),
      {
        tag,
        connectionId: entry.connectionId,
        bootGeneration: entry.bootGeneration!.toString(),
        notificationId: entry.item.notificationId,
      },
    );
    void postWorker({
      type: "blit-desktop-notification-show",
      tag,
      connectionId: entry.connectionId,
      bootGeneration: entry.bootGeneration!.toString(),
      notificationId: entry.item.notificationId,
      revision: entry.item.revision,
      title: notificationTitle(entry.item),
      body: entry.item.body,
      icon: imageUrl(entry.item.icon),
      image: imageUrl(entry.item.image),
    });
  };

  const raise = (entry: NotificationEntry) => {
    const delivery = desktopDelivery(document.visibilityState, permission());
    if (delivery === "native") {
      showNative(entry);
      return;
    }
    if (delivery === "toast") {
      const key = notificationKey(
        entry.connectionId,
        entry.item.notificationId,
      );
      setToasts((items) => [
        ...items.filter((item) => item.key !== key),
        { ...entry, key },
      ]);
      const previous = toastTimers.get(key);
      if (previous) clearTimeout(previous);
      toastTimers.set(
        key,
        setTimeout(
          () => {
            setToasts((items) => items.filter((item) => item.key !== key));
            toastTimers.delete(key);
          },
          entry.item.urgency === 2 ? 10_000 : 6_000,
        ),
      );
    }
  };

  createEffect(() => {
    const cleanups: (() => void)[] = [];
    for (const snapshot of props.connections) {
      const connection = props.workspace.getConnection(snapshot.id);
      if (!snapshot.supportsDesktop || !connection) continue;
      const label = props.connectionLabels.get(snapshot.id) ?? "";
      const readOnly = props.readOnlyConnections.has(snapshot.id);
      cleanups.push(
        connection.desktopStore.onNotificationRaised((item) =>
          raise({
            connectionId: snapshot.id,
            connectionLabel: label,
            bootGeneration: snapshot.bootGeneration,
            readOnly,
            item,
          }),
        ),
        connection.desktopStore.onTrayMenu((next) => {
          const entry = tray().find(
            (candidate) =>
              candidate.connectionId === snapshot.id &&
              candidate.item.trayId === next.trayId,
          );
          if (next.status === TRAY_MENU_OK && entry) {
            setMenu({ entry, menu: next });
          } else if (next.status !== TRAY_MENU_OK) {
            setMenu(null);
          }
        }),
      );
    }
    onCleanup(() => cleanups.forEach((cleanup) => cleanup()));
  });

  createEffect(() => {
    const active = new Set(
      notifications().map(
        (entry) =>
          `${entry.connectionId}:${entry.bootGeneration}:${entry.item.notificationId}:${entry.item.revision}`,
      ),
    );
    setToasts((items) =>
      items.filter((entry) =>
        active.has(
          `${entry.connectionId}:${entry.bootGeneration}:${entry.item.notificationId}:${entry.item.revision}`,
        ),
      ),
    );
    for (const [key, shown] of nativeShown) {
      const current = notifications().find(
        (entry) =>
          entry.connectionId === shown.connectionId &&
          String(entry.bootGeneration) === shown.bootGeneration &&
          entry.item.notificationId === shown.notificationId,
      );
      if (!current) {
        nativeShown.delete(key);
        void postWorker({
          type: "blit-desktop-notification-close",
          tag: shown.tag,
        });
      }
    }
  });

  const openTrayMenu = (entry: TrayEntry, parentId = 0) => {
    if (entry.readOnly) return;
    setTrayOpen(false);
    setBellOpen(false);
    props.workspace
      .getConnection(entry.connectionId)
      ?.desktopStore.openMenu(
        entry.item.trayId,
        menu()?.entry.item.trayId === entry.item.trayId
          ? menu()!.menu.menuRevision
          : 0,
        parentId,
      );
  };
  const activateTray = (entry: TrayEntry, touch = false) => {
    if (entry.readOnly) return;
    const store = props.workspace.getConnection(
      entry.connectionId,
    )?.desktopStore;
    if (trayPrimaryOpensMenu(entry.item.flags, touch)) {
      openTrayMenu(entry);
    } else {
      store?.activate(entry.item.trayId);
    }
  };

  onMount(() => {
    const pointer = (event: PointerEvent) => {
      if (root && !root.contains(event.target as Node)) {
        setBellOpen(false);
        setTrayOpen(false);
        setMenu(null);
      }
    };
    const key = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setBellOpen(false);
      setTrayOpen(false);
      setMenu(null);
    };
    const worker = (event: MessageEvent) => {
      const data = event.data as {
        type?: string;
        connectionId?: string;
        bootGeneration?: string;
        notificationId?: number;
        revision?: number;
      } | null;
      if (data?.type !== "blit-desktop-notification-click") return;
      const entry = notifications().find(
        (candidate) =>
          candidate.connectionId === data.connectionId &&
          candidate.bootGeneration?.toString() === data.bootGeneration &&
          matchesDesktopNotification(candidate.item, data),
      );
      if (
        entry &&
        entry.item.actions.some((action) => action.key === "default")
      ) {
        invoke(entry, null);
      }
    };
    document.addEventListener("pointerdown", pointer, true);
    document.addEventListener("keydown", key, true);
    navigator.serviceWorker?.addEventListener("message", worker);
    onCleanup(() => {
      document.removeEventListener("pointerdown", pointer, true);
      document.removeEventListener("keydown", key, true);
      navigator.serviceWorker?.removeEventListener("message", worker);
      toastTimers.forEach(clearTimeout);
    });
  });

  const trayButton = (entry: TrayEntry): JSX.Element => {
    let primaryPointerType: string | null = null;
    const icon = imageUrl(entry.item.icon);
    const title = [
      entry.item.tooltipTitle || entry.item.title || entry.item.appId,
      entry.item.tooltipBody,
      entry.connectionLabel,
    ]
      .filter(Boolean)
      .join("\n");
    return (
      <button
        disabled={entry.readOnly}
        onPointerDown={(event) => {
          primaryPointerType = event.pointerType;
        }}
        onPointerCancel={() => {
          primaryPointerType = null;
        }}
        onClick={() => {
          const touch = primaryPointerType === "touch";
          primaryPointerType = null;
          activateTray(entry, touch);
        }}
        onContextMenu={(event) => {
          primaryPointerType = null;
          event.preventDefault();
          openTrayMenu(entry);
        }}
        onAuxClick={(event) => {
          if (event.button !== 1 || entry.readOnly) return;
          props.workspace
            .getConnection(entry.connectionId)
            ?.desktopStore.secondaryActivate(entry.item.trayId);
        }}
        onWheel={(event) => {
          if (entry.readOnly) return;
          event.preventDefault();
          const horizontal = Math.abs(event.deltaX) > Math.abs(event.deltaY);
          const raw = horizontal ? event.deltaX : event.deltaY;
          props.workspace
            .getConnection(entry.connectionId)
            ?.desktopStore.scroll(
              entry.item.trayId,
              Math.max(-1_000, Math.min(1_000, Math.trunc(raw))),
              horizontal,
            );
        }}
        title={title}
        aria-label={title || t("desktop.trayItem")}
        aria-haspopup={
          trayPrimaryOpensMenu(entry.item.flags, true) ? "menu" : undefined
        }
        style={mergeStyle(ui.btn, {
          width: `${props.scale.icon / 2}px`,
          height: `${props.scale.icon / 2}px`,
          display: "grid",
          "place-items": "center",
          padding: "2px",
          "border-radius": "3px",
          "background-color":
            entry.item.status === TRAY_STATUS_NEEDS_ATTENTION
              ? props.theme.warning
              : "transparent",
          opacity: entry.readOnly ? 0.5 : 1,
          "touch-action": "manipulation",
          "-webkit-touch-callout": "none",
        })}
      >
        <Show when={icon} fallback={<span>●</span>}>
          <img
            src={icon}
            alt=""
            draggable={false}
            style={{
              width: "100%",
              height: "100%",
              "object-fit": "contain",
              "pointer-events": "none",
            }}
          />
        </Show>
      </button>
    );
  };

  return (
    <span
      ref={root}
      data-blit-desktop-chrome=""
      style={{ display: "flex", "align-items": "center", position: "relative" }}
    >
      {/* Camera and microphone are not here: their controls, their preview
          and their privacy indicator all belong to the media panel, which
          costs the bar one glyph instead of a row of chips. See
          `mediaDevices.ts` and the `media` entry in StatusBar's tools. */}
      <PortalChrome
        workspace={props.workspace}
        connections={props.connections}
        connectionLabels={props.connectionLabels}
        readOnlyConnections={props.readOnlyConnections}
        theme={props.theme}
        scale={props.scale}
      />
      <MprisChrome
        workspace={props.workspace}
        connections={props.connections}
        connectionLabels={props.connectionLabels}
        readOnlyConnections={props.readOnlyConnections}
        theme={props.theme}
        scale={props.scale}
        compact={props.compact}
        focusedConnectionId={props.focusedConnectionId}
        closeOthers={() => {
          setBellOpen(false);
          setTrayOpen(false);
          setMenu(null);
        }}
      />
      <For each={visibleTray().slice(0, props.compact ? 0 : 4)}>
        {trayButton}
      </For>
      <Show when={overflowTray().length > 0}>
        <button
          onClick={() => {
            setTrayOpen((open) => !open);
            setBellOpen(false);
          }}
          title={t("desktop.trayOverflow")}
          aria-label={t("desktop.trayOverflow")}
          aria-haspopup="menu"
          aria-expanded={trayOpen()}
          style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
        >
          ◉{overflowTray().length}
        </button>
        <Show when={trayOpen()}>
          <Popup theme={props.theme} scale={props.scale} width="18em">
            <div role="menu" style={{ padding: `${props.scale.tightGap}px` }}>
              <For each={overflowTray()}>
                {(entry) => (
                  <div
                    style={{
                      display: "flex",
                      "align-items": "center",
                      gap: `${props.scale.gap}px`,
                    }}
                  >
                    {trayButton(entry)}
                    <span
                      style={{ "min-width": 0, "overflow-wrap": "anywhere" }}
                    >
                      {entry.item.title ||
                        entry.item.appId ||
                        t("desktop.trayItem")}
                      <Show when={entry.connectionLabel}>
                        <small
                          style={{ display: "block", color: props.theme.dimFg }}
                        >
                          {entry.connectionLabel}
                        </small>
                      </Show>
                    </span>
                  </div>
                )}
              </For>
            </div>
          </Popup>
        </Show>
      </Show>
      <Show
        when={
          desktopEnabled() &&
          (notifications().length > 0 || permission() !== "granted")
        }
      >
        <button
          onClick={() => {
            setBellOpen((open) => !open);
            setTrayOpen(false);
          }}
          title={t("desktop.notifications")}
          aria-label={t("desktop.notifications")}
          aria-haspopup="menu"
          aria-expanded={bellOpen()}
          style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
        >
          ♢
          <Show when={notifications().length > 0}>
            {notifications().length}
          </Show>
        </button>
        <Show when={bellOpen()}>
          <Popup
            theme={props.theme}
            scale={props.scale}
            width="min(21em, calc(100vw - 1.5em))"
            maxHeight="min(60vh, 30em)"
          >
            <Show when={notifications().length > 1}>
              <header
                style={{
                  display: "flex",
                  "align-items": "center",
                  "justify-content": "space-between",
                  gap: `${props.scale.gap}px`,
                  padding: `${props.scale.tightGap}px ${props.scale.gap}px`,
                  "border-bottom": `1px solid ${props.theme.subtleBorder}`,
                  color: props.theme.dimFg,
                  "font-size": `${props.scale.sm}px`,
                }}
              >
                <span>
                  {tp("desktop.notificationCount", {
                    count: notifications().length,
                  })}
                </span>
                <button
                  onClick={() => notifications().forEach(dismiss)}
                  style={mergeStyle(ui.btn, {
                    "font-size": "inherit",
                    padding: `0 ${props.scale.tightGap}px`,
                  })}
                >
                  {t("desktop.dismissAll")}
                </button>
              </header>
            </Show>
            <Show
              when={notifications().length > 0}
              fallback={
                <p
                  style={{
                    margin: 0,
                    padding: `${props.scale.gap}px`,
                    color: props.theme.dimFg,
                  }}
                >
                  {t("desktop.noNotifications")}
                </p>
              }
            >
              <For each={notifications()}>
                {(entry) => (
                  <NotificationCard
                    entry={entry}
                    theme={props.theme}
                    scale={props.scale}
                    invoke={(action) => invoke(entry, action)}
                    dismiss={() => dismiss(entry)}
                  />
                )}
              </For>
            </Show>
            <Show when={permission() === "default"}>
              <button
                onClick={async () => {
                  if (typeof Notification === "undefined") return;
                  const result = await Notification.requestPermission();
                  setPermission(result);
                  if (result === "granted") await desktopWorkerRegistration();
                }}
                style={mergeStyle(ui.btn, {
                  display: "block",
                  width: "100%",
                  padding: `${props.scale.tightGap}px ${props.scale.gap}px`,
                  "border-top": `1px solid ${props.theme.subtleBorder}`,
                  "font-size": `${props.scale.sm}px`,
                  color: props.theme.dimFg,
                  "text-align": "left",
                })}
              >
                {t("desktop.enableSystemNotifications")}
              </button>
            </Show>
            <Show when={permission() === "denied"}>
              <p
                style={{
                  margin: 0,
                  padding: `${props.scale.tightGap}px ${props.scale.gap}px`,
                  "border-top": `1px solid ${props.theme.subtleBorder}`,
                  "font-size": `${props.scale.sm}px`,
                  color: props.theme.dimFg,
                }}
              >
                {t("desktop.systemNotificationsBlocked")}
              </p>
            </Show>
          </Popup>
        </Show>
      </Show>
      <Show when={menu()} keyed>
        {(state) => (
          <Popup theme={props.theme} scale={props.scale} width="20em">
            <MenuNodes
              nodes={state.menu.nodes}
              parentId={0}
              depth={0}
              readOnly={state.entry.readOnly}
              theme={props.theme}
              scale={props.scale}
              openSubmenu={(id) => openTrayMenu(state.entry, id)}
              click={(id) => {
                props.workspace
                  .getConnection(state.entry.connectionId)
                  ?.desktopStore.clickMenuItem(
                    state.entry.item.trayId,
                    state.menu.menuRevision,
                    id,
                  );
                setMenu(null);
              }}
            />
          </Popup>
        )}
      </Show>
      <Portal>
        <div
          aria-live="polite"
          style={{
            position: "fixed",
            right: "1em",
            bottom: "3em",
            width: "min(24em, calc(100vw - 2em))",
            display: "flex",
            "flex-direction": "column",
            gap: `${props.scale.gap}px`,
            "z-index": z.disconnected,
            "pointer-events": "none",
          }}
        >
          <For each={toasts()}>
            {(toast) => (
              <div
                role="status"
                style={{
                  border: `1px solid ${props.theme.border}`,
                  "box-shadow": "0 8px 24px rgba(0,0,0,0.35)",
                  "pointer-events": "auto",
                }}
              >
                <NotificationCard
                  entry={toast}
                  theme={props.theme}
                  scale={props.scale}
                  toast
                  invoke={(key) => invoke(toast, key)}
                  dismiss={() => dismiss(toast)}
                />
              </div>
            )}
          </For>
        </div>
      </Portal>
    </span>
  );
}
