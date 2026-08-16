import { createSignal, Index, Show } from "solid-js";
import type {
  BlitConnectionSnapshot,
  BlitSession,
  BlitSurface,
  BlitWorkspace,
  ConnectionId,
  ConnectionStatus,
  TerminalPalette,
} from "@blit-sh/core";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { mergeStyle, scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import { createDragReorder, reorderTo } from "./dragReorder";
import {
  ConnectionClients,
  connectionHasClientList,
} from "./ConnectionClients";
import { t } from "./i18n";
import type { Remote } from "./storage";

/** Returns true if the URI scheme is share: (contains a secret passphrase). */
function isShareUri(uri: string): boolean {
  return uri.trimStart().toLowerCase().startsWith("share:");
}

const STATUS_COLORS: Record<string, string> = {
  connected: "#4caf50",
  connecting: "#ff9800",
  authenticating: "#ff9800",
  disconnected: "#888",
  closed: "#888",
  error: "#f44336",
};

export function RemotesOverlay(props: {
  remotes: Remote[];
  defaultRemote: string | null;
  statuses?: ReadonlyMap<string, ConnectionStatus>;
  gatewayStatus?: "connecting" | "connected" | "unavailable";
  palette: TerminalPalette;
  fontSize: number;
  /** When true, show connection statuses only — no add/remove/reorder actions. */
  readOnly?: boolean;
  onAdd: (name: string, uri: string) => void;
  onRemove: (name: string) => void;
  onToggle?: (name: string) => void;
  onSetDefault: (name: string) => void;
  onReorder: (names: string[]) => void;
  onReconnect?: (name: string) => void;
  onClose: () => void;
  /** Live connections, used to decide which rows can list their clients.
   *  Omit (with `workspace`) to render the remotes list on its own. */
  connections?: readonly BlitConnectionSnapshot[];
  workspace?: BlitWorkspace;
  sessions?: readonly BlitSession[];
  surfaces?: readonly BlitSurface[];
  readOnlyConnections?: ReadonlySet<ConnectionId>;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [name, setName] = createSignal("");
  const [uri, setUri] = createSignal("");
  const [revealed, setRevealed] = createSignal<Set<string>>(new Set());
  // At most one remote's client list is open at a time: each open list holds a
  // live CLIENT_WATCH pushing a catalog every second, and reading two servers'
  // clients side by side is not a thing anyone does.
  const [expanded, setExpanded] = createSignal<string | null>(null);

  /** A remote's live connection, if it has one. Remote names *are* connection
   *  ids (App.tsx builds one ConnectionSpec per enabled remote, `id: name`). */
  const connectionFor = (remoteName: string) =>
    props.connections?.find((c) => c.id === remoteName);

  /** Whether this row can show a client list right now. */
  const canListClients = (remoteName: string) => {
    const connection = connectionFor(remoteName);
    return (
      !!props.workspace &&
      !!connection &&
      connectionHasClientList(
        connection,
        props.readOnlyConnections ?? new Set(),
      )
    );
  };

  function toggleExpanded(remoteName: string) {
    setExpanded((prev) => (prev === remoteName ? null : remoteName));
  }

  /** Any row at all offers a client list — gates the header's explanation of
   *  what the chevrons are, so a shell without client control says nothing. */
  const anyClients = () => props.remotes.some((r) => canListClients(r.name));

  let nameRef!: HTMLInputElement;

  function toggleReveal(remoteName: string) {
    setRevealed((prev) => {
      const next = new Set(prev);
      if (next.has(remoteName)) next.delete(remoteName);
      else next.add(remoteName);
      return next;
    });
  }

  function handleAdd(e: SubmitEvent) {
    e.preventDefault();
    const n = name().trim();
    const u = uri().trim();
    if (!n || !u) return;
    props.onAdd(n, u);
    setName("");
    setUri("");
    nameRef?.focus();
  }

  const inputStyle = () => ({
    ...ui.input,
    "background-color": theme().inputBg,
    color: "inherit",
    "font-size": `${scale().md}px`,
    "border-radius": "0",
    flex: 1,
    "min-width": "0",
  });

  const btnStyle = () => ({
    ...ui.btn,
    "font-size": `${scale().sm}px`,
    "border-radius": "0",
    border: "none",
    "background-color": "transparent",
    color: "inherit",
    padding: `${scale().controlY}px ${scale().controlX + 2}px`,
    cursor: "pointer",
    "white-space": "nowrap",
    opacity: 0.7,
  });

  const hasShare = () => props.remotes.some((r) => isShareUri(r.uri));
  // Mutating remotes (add/remove/reorder/set-default/reconnect) requires
  // the config WebSocket to round-trip `set remotes …` to the gateway.
  // Block and visually disable these controls while the gateway handshake
  // is still in progress or unreachable.  When gatewayStatus is undefined
  // (no config WS wired up), allow mutations — local-only callers.
  const mutationsBlocked = () =>
    props.gatewayStatus !== undefined && props.gatewayStatus !== "connected";

  // Reordering runs on pointer events, not HTML5 drag-and-drop, so the drag
  // handle works under touch as well as a mouse.
  const drag = createDragReorder({
    count: () => props.remotes.length,
    disabled: () => !!props.readOnly || mutationsBlocked(),
    onDrop: (from, gap) => {
      const names = reorderTo(
        props.remotes.map((r) => r.name),
        from,
        gap,
      );
      if (names) props.onReorder(names);
    },
  });

  // Columns: drag, name, uri, default, [reveal], [clients], reconnect,
  // toggle, remove. Optional columns are only present when some row can fill
  // them — a share URI for reveal, a client-listing connection for clients.
  const cols = () => {
    if (props.readOnly) return "auto 1fr";
    return [
      "auto",
      "auto",
      "1fr",
      "auto",
      hasShare() ? "auto" : null,
      anyClients() ? "auto" : null,
      "auto",
      "auto",
      "auto",
    ]
      .filter(Boolean)
      .join(" ");
  };

  return (
    <OverlayBackdrop
      palette={props.palette}
      label={t("remotes.label")}
      onClose={props.onClose}
    >
      <OverlayPanel
        palette={props.palette}
        fontSize={props.fontSize}
        style={{
          display: "flex",
          "flex-direction": "column",
          gap: `${scale().gap}px`,
          width: "fit-content",
          // An expanded client list is much wider than a remotes row, and
          // fit-content would happily follow it off the screen.
          "max-width": "min(860px, 94vw)",
        }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title={
            props.readOnly ? t("remotes.connectingTitle") : t("remotes.title")
          }
          subtitle={anyClients() ? t("remotes.clientsSubtitle") : undefined}
          onClose={props.onClose}
        />

        {/* Gateway status — only shown while not yet connected */}
        <Show
          when={
            props.gatewayStatus && props.gatewayStatus !== "connected"
              ? props.gatewayStatus
              : undefined
          }
        >
          {(gw) => {
            const color = () =>
              gw() === "connecting"
                ? STATUS_COLORS.connecting
                : STATUS_COLORS.error;
            return (
              <div
                style={{
                  display: "flex",
                  "align-items": "center",
                  gap: `${scale().tightGap}px`,
                  padding: `${scale().controlY}px ${scale().controlX}px`,
                  border: `1px solid ${theme().subtleBorder}`,
                  "background-color": theme().solidPanelBg,
                  "font-size": `${scale().md}px`,
                }}
              >
                <span
                  title={t(`remotes.gateway.${gw()}`)}
                  style={{
                    display: "inline-block",
                    width: "8px",
                    height: "8px",
                    "border-radius": "50%",
                    "background-color": color(),
                    "flex-shrink": 0,
                  }}
                />
                <span style={{ "font-weight": 600 }}>
                  {t("remotes.gateway")}
                </span>
                <span
                  style={{
                    "font-size": `${scale().sm}px`,
                    color: theme().dimFg,
                  }}
                >
                  {t(`remotes.gateway.${gw()}`)}
                </span>
              </div>
            );
          }}
        </Show>

        {/* Existing remotes list */}
        <Show
          when={props.remotes.length > 0}
          fallback={
            <div
              style={{
                padding: `${scale().panelPadding}px`,
                border: `1px dashed ${theme().subtleBorder}`,
                "text-align": "center",
                color: theme().dimFg,
                "font-size": `${scale().sm}px`,
                display: "grid",
                gap: `${scale().tightGap}px`,
              }}
            >
              <div
                style={{ "font-size": `${scale().md}px`, color: theme().fg }}
              >
                {t("remotes.empty")}
              </div>
              <Show when={!mutationsBlocked()}>
                <div>{t("remotes.emptyHint")}</div>
              </Show>
            </div>
          }
        >
          <div
            role="list"
            ref={drag.containerRef}
            style={{
              display: "grid",
              "grid-template-columns": cols(),
              "max-height": "60vh",
              "overflow-y": "auto",
              ...scrollbarStyle(theme()),
            }}
          >
            <Index each={props.remotes}>
              {(remote, index) => {
                const share = () => isShareUri(remote().uri);
                const show = () => revealed().has(remote().name);
                const disabled = () => remote().disabled;
                const effectiveDefault = () =>
                  props.defaultRemote && props.defaultRemote !== "local"
                    ? props.defaultRemote
                    : "local";
                const isDefault = () => remote().name === effectiveDefault();
                const displayUri = () =>
                  share() && !show()
                    ? "share:\u2022\u2022\u2022\u2022"
                    : remote().uri;
                const status = () =>
                  disabled()
                    ? null
                    : (props.statuses?.get(remote().name) ?? null);
                const statusColor = () => {
                  const s = status();
                  return s
                    ? (STATUS_COLORS[s] ?? theme().dimFg)
                    : theme().dimFg;
                };

                const rowOpacity = () =>
                  drag.sourceIndex() === index ? 0.5 : 1;
                const showGapBefore = () => {
                  const gap = drag.dropGap();
                  return gap === index && drag.wouldMove(gap);
                };
                const showGapAfter = () => {
                  const gap = drag.dropGap();
                  return (
                    gap === index + 1 &&
                    index === props.remotes.length - 1 &&
                    drag.wouldMove(gap)
                  );
                };

                const clients = () => canListClients(remote().name);
                const open = () => expanded() === remote().name;

                return (
                  <>
                    <div
                      role="listitem"
                      ref={drag.rowRef(index)}
                      onPointerDown={(e) => drag.onRowPointerDown(e, index)}
                      style={{
                        display: "grid",
                        "grid-template-columns": "subgrid",
                        "grid-column": "1 / -1",
                        "align-items": "center",
                        "border-top": showGapBefore()
                          ? `2px solid ${theme().accent}`
                          : index > 0
                            ? "none"
                            : `1px solid ${theme().subtleBorder}`,
                        "border-bottom": showGapAfter()
                          ? `2px solid ${theme().accent}`
                          : `1px solid ${theme().subtleBorder}`,
                        "border-left": `1px solid ${theme().subtleBorder}`,
                        "border-right": `1px solid ${theme().subtleBorder}`,
                        "background-color": theme().solidPanelBg,
                        opacity: rowOpacity() * (disabled() ? 0.55 : 1),
                        transition: "opacity 0.1s",
                      }}
                    >
                      {/* Drag handle */}
                      <Show when={!props.readOnly}>
                        <div
                          title={t("remotes.dragHandle")}
                          aria-label={t("remotes.dragHandle")}
                          onPointerDown={(e) =>
                            drag.onHandlePointerDown(e, index)
                          }
                          style={{
                            display: "flex",
                            "align-items": "center",
                            "align-self": "stretch",
                            "justify-content": "center",
                            padding: `0 ${scale().controlX + 4}px`,
                            cursor: mutationsBlocked()
                              ? "not-allowed"
                              : drag.sourceIndex() === index
                                ? "grabbing"
                                : "grab",
                            color: theme().dimFg,
                            "font-size": `${scale().md}px`,
                            "user-select": "none",
                            // Claim the gesture from the container's touch
                            // panning, so a finger on the handle reorders.
                            "touch-action": "none",
                            "border-right": `1px solid ${theme().subtleBorder}`,
                            opacity: mutationsBlocked() ? 0.4 : 1,
                          }}
                        >
                          ⠿
                        </div>
                      </Show>

                      {/* Status dot + Name */}
                      <div
                        style={{
                          padding: `${scale().controlY}px ${scale().controlX}px`,
                          "font-size": `${scale().md}px`,
                          "font-weight": 600,
                          display: "flex",
                          "align-items": "center",
                          gap: `${scale().tightGap}px`,
                          "white-space": "nowrap",
                        }}
                      >
                        <span
                          title={
                            status() ? t(`remotes.status.${status()}`) : ""
                          }
                          style={{
                            display: "inline-block",
                            width: "8px",
                            height: "8px",
                            "border-radius": "50%",
                            "background-color": statusColor(),
                            "flex-shrink": 0,
                          }}
                        />
                        {remote().name}
                      </div>

                      {/* URI */}
                      <Show when={!props.readOnly}>
                        <div
                          style={{
                            padding: `${scale().controlY}px ${scale().controlX}px`,
                            "font-size": `${scale().sm}px`,
                            color:
                              share() && !show() ? theme().dimFg : theme().fg,
                            overflow: "hidden",
                            "text-overflow": "ellipsis",
                            "white-space": "nowrap",
                            "font-family":
                              share() && !show()
                                ? "inherit"
                                : "monospace, inherit",
                            "letter-spacing":
                              share() && !show() ? "0.05em" : "normal",
                          }}
                        >
                          {displayUri()}
                        </div>

                        {/* Default / Set as default */}
                        <Show
                          when={isDefault()}
                          fallback={
                            <button
                              type="button"
                              title={t("remotes.setDefault")}
                              disabled={mutationsBlocked()}
                              onClick={() => props.onSetDefault(remote().name)}
                              style={{
                                ...btnStyle(),
                                opacity: mutationsBlocked() ? 0.3 : 0.5,
                                cursor: mutationsBlocked()
                                  ? "not-allowed"
                                  : "pointer",
                                "border-left": `1px solid ${theme().subtleBorder}`,
                              }}
                            >
                              {t("remotes.setDefault")}
                            </button>
                          }
                        >
                          <div
                            title={t("remotes.isDefault")}
                            style={{
                              ...btnStyle(),
                              cursor: "default",
                              color: theme().accent,
                              "border-left": `1px solid ${theme().subtleBorder}`,
                            }}
                          >
                            {t("remotes.isDefault")}
                          </div>
                        </Show>

                        {/* Reveal/hide — only column present when any remote is share */}
                        <Show when={hasShare()}>
                          <Show when={share()} fallback={<div />}>
                            <button
                              type="button"
                              title={
                                show()
                                  ? t("remotes.hideUri")
                                  : t("remotes.revealUri")
                              }
                              onClick={() => toggleReveal(remote().name)}
                              style={btnStyle()}
                            >
                              {show()
                                ? t("remotes.hideUri")
                                : t("remotes.revealUri")}
                            </button>
                          </Show>
                        </Show>

                        {/* Clients — a named action rather than a bare
                            chevron on the name: this replaced a top-level
                            "Connected clients" entry in the command palette,
                            and a 1-em glyph is not a discoverable home for
                            something that used to have its own menu item.
                            Only offered where the remote could actually
                            answer; a disconnected row that expanded to "No
                            clients connected" would be reporting the wrong
                            thing. */}
                        <Show when={anyClients()}>
                          <Show
                            when={clients()}
                            fallback={
                              <div
                                title={
                                  // Two different reasons, and they call for
                                  // different answers: connect the remote, or
                                  // accept that this server is too old to say.
                                  connectionFor(remote().name)?.status ===
                                  "connected"
                                    ? t("remotes.clientsUnsupported")
                                    : t("remotes.clientsDisconnected")
                                }
                                style={{ ...btnStyle(), opacity: 0.25 }}
                              >
                                {t("remotes.clients")}
                              </div>
                            }
                          >
                            <button
                              type="button"
                              aria-expanded={open()}
                              title={
                                open()
                                  ? t("remotes.hideClients")
                                  : t("remotes.showClients")
                              }
                              onClick={() => toggleExpanded(remote().name)}
                              style={{
                                ...btnStyle(),
                                opacity: open() ? 1 : 0.7,
                                color: open() ? theme().accent : "inherit",
                              }}
                            >
                              {open() ? "▾ " : "▸ "}
                              {t("remotes.clients")}
                            </button>
                          </Show>
                        </Show>

                        {/* Reconnect — hidden for disabled entries */}
                        <Show when={!disabled()} fallback={<div />}>
                          <button
                            type="button"
                            title={t("disconnected.reconnectNow")}
                            disabled={mutationsBlocked()}
                            onClick={() => props.onReconnect?.(remote().name)}
                            style={{
                              ...btnStyle(),
                              opacity: mutationsBlocked() ? 0.3 : 0.7,
                              cursor: mutationsBlocked()
                                ? "not-allowed"
                                : "pointer",
                            }}
                          >
                            {t("disconnected.reconnectNow")}
                          </button>
                        </Show>

                        {/* Disable / Enable */}
                        <button
                          type="button"
                          title={
                            disabled()
                              ? t("remotes.enable")
                              : t("remotes.disable")
                          }
                          disabled={mutationsBlocked() || !props.onToggle}
                          onClick={() => props.onToggle?.(remote().name)}
                          style={{
                            ...btnStyle(),
                            opacity:
                              mutationsBlocked() || !props.onToggle ? 0.3 : 0.7,
                            cursor:
                              mutationsBlocked() || !props.onToggle
                                ? "not-allowed"
                                : "pointer",
                          }}
                        >
                          {disabled()
                            ? t("remotes.enable")
                            : t("remotes.disable")}
                        </button>

                        {/* Remove */}
                        <button
                          type="button"
                          title={t("remotes.remove")}
                          disabled={mutationsBlocked()}
                          onClick={() => props.onRemove(remote().name)}
                          style={{
                            ...btnStyle(),
                            opacity: mutationsBlocked() ? 0.3 : 0.7,
                            cursor: mutationsBlocked()
                              ? "not-allowed"
                              : "pointer",
                          }}
                        >
                          {t("remotes.remove")}
                        </button>
                      </Show>
                    </div>

                    {/* Clients, as a full-width row of the same grid. A sibling
                      of the remote row rather than a child of it, so the
                      drag-reorder extents stay the height of the header the
                      user is actually dragging. */}
                    <Show when={clients() && open()}>
                      <div
                        style={{
                          "grid-column": "1 / -1",
                          // Keeps the client list from widening the grid's
                          // columns; it wraps instead.
                          "min-width": "0",
                          "border-left": `1px solid ${theme().subtleBorder}`,
                          "border-right": `1px solid ${theme().subtleBorder}`,
                          "border-bottom": `1px solid ${theme().subtleBorder}`,
                        }}
                      >
                        <ConnectionClients
                          workspace={props.workspace!}
                          connectionId={remote().name as ConnectionId}
                          sessions={props.sessions ?? []}
                          surfaces={props.surfaces ?? []}
                          palette={props.palette}
                          fontSize={props.fontSize}
                        />
                      </div>
                    </Show>
                  </>
                );
              }}
            </Index>
          </div>
        </Show>

        <Show when={!props.readOnly && !mutationsBlocked()}>
          {/* share: warning */}
          <Show when={hasShare()}>
            <div
              style={{
                "font-size": `${scale().xs}px`,
                color: theme().dimFg,
                padding: `${scale().tightGap}px ${scale().controlX}px`,
                border: `1px solid ${theme().subtleBorder}`,
                "background-color": theme().panelBg,
              }}
            >
              {t("remotes.shareWarning")}
            </div>
          </Show>

          {/* Add form */}
          <form
            onSubmit={handleAdd}
            style={{
              display: "flex",
              gap: `${scale().tightGap}px`,
              "align-items": "stretch",
              "border-top": `1px solid ${theme().subtleBorder}`,
              "padding-top": `${scale().gap}px`,
            }}
          >
            <input
              ref={nameRef}
              name="blit-remote-name"
              type="text"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
              placeholder={t("remotes.namePlaceholder")}
              autocomplete="off"
              autocorrect="off"
              autocapitalize="off"
              spellcheck={false}
              style={{
                ...inputStyle(),
                flex: "0 0 8em",
                "font-weight": 600,
              }}
            />
            <input
              name="blit-remote-uri"
              type="text"
              value={uri()}
              onInput={(e) => setUri(e.currentTarget.value)}
              placeholder={t("remotes.uriPlaceholder")}
              autocomplete="off"
              autocorrect="off"
              autocapitalize="off"
              spellcheck={false}
              style={inputStyle()}
            />
            <button
              type="submit"
              disabled={!name().trim() || !uri().trim()}
              style={mergeStyle(ui.btn, {
                "font-size": `${scale().sm}px`,
                "border-radius": "0",
                border: `1px solid ${theme().accent}`,
                "background-color": theme().accent,
                color: "#fff",
                padding: `${scale().controlY}px ${scale().controlX + 2}px`,
                "flex-shrink": 0,
                cursor: "pointer",
                "white-space": "nowrap",
                opacity: name().trim() && uri().trim() ? 1 : 0.4,
              })}
            >
              {t("remotes.add")}
            </button>
          </form>
        </Show>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
