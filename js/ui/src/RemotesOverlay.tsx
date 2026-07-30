import { createSignal, Index, Show } from "solid-js";
import type { ConnectionStatus, TerminalPalette } from "@blit-sh/core";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import { createDragReorder, reorderTo } from "./dragReorder";
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
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [name, setName] = createSignal("");
  const [uri, setUri] = createSignal("");
  const [revealed, setRevealed] = createSignal<Set<string>>(new Set());

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

  // Only include the reveal/hide column if any remote is a share URI.
  // Columns: drag, name, uri, default, [reveal], reconnect, toggle, remove.
  const cols = () => {
    if (props.readOnly) return "auto 1fr";
    return hasShare()
      ? "auto auto 1fr auto auto auto auto auto"
      : "auto auto 1fr auto auto auto auto";
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
        }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title={
            props.readOnly ? t("remotes.connectingTitle") : t("remotes.title")
          }
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

                return (
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
                        title={status() ? t(`remotes.status.${status()}`) : ""}
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
              style={{
                ...ui.btn,
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
              }}
            >
              {t("remotes.add")}
            </button>
          </form>
        </Show>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
