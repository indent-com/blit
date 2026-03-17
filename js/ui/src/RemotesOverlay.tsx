import { createSignal, For, Index, Show } from "solid-js";
import type { ConnectionStatus, TerminalPalette } from "@blit-sh/core";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { themeFor, ui, uiScale } from "./theme";
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
  palette: TerminalPalette;
  fontSize: number;
  onAdd: (name: string, uri: string) => void;
  onRemove: (name: string) => void;
  onSetDefault: (name: string) => void;
  onReorder: (names: string[]) => void;
  onClose: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [name, setName] = createSignal("");
  const [uri, setUri] = createSignal("");
  // Per-remote reveal state: set of names whose URIs are currently shown.
  const [revealed, setRevealed] = createSignal<Set<string>>(new Set());

  // Drag-and-drop state
  const [dragOverIndex, setDragOverIndex] = createSignal<number | null>(null);
  let dragSourceIndex: number | null = null;

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

  function handleDragStart(e: DragEvent, index: number) {
    dragSourceIndex = index;
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", String(index));
    }
  }

  function handleDragOver(e: DragEvent, index: number) {
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    setDragOverIndex(index);
  }

  function handleDragLeave() {
    setDragOverIndex(null);
  }

  function handleDrop(e: DragEvent, targetIndex: number) {
    e.preventDefault();
    setDragOverIndex(null);
    if (dragSourceIndex === null || dragSourceIndex === targetIndex) return;
    const names = props.remotes.map((r) => r.name);
    const [moved] = names.splice(dragSourceIndex, 1);
    names.splice(targetIndex, 0, moved);
    props.onReorder(names);
    dragSourceIndex = null;
  }

  function handleDragEnd() {
    dragSourceIndex = null;
    setDragOverIndex(null);
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
    border: `1px solid ${theme().subtleBorder}`,
    "background-color": theme().inputBg,
    color: "inherit",
    padding: `${scale().controlY}px ${scale().controlX + 2}px`,
    "flex-shrink": 0,
    cursor: "pointer",
    "white-space": "nowrap",
  });

  const hasShare = () => props.remotes.some((r) => isShareUri(r.uri));

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
          "min-width": "28em",
          "max-width": "42em",
          width: "90vw",
        }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title={t("remotes.title")}
          subtitle={t("remotes.subtitle")}
          onClose={props.onClose}
        />

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
              <div>{t("remotes.emptyHint")}</div>
            </div>
          }
        >
          <div
            role="list"
            style={{
              display: "grid",
              gap: `${scale().tightGap}px`,
              "max-height": "60vh",
              "overflow-y": "auto",
            }}
          >
            <Index each={props.remotes}>
              {(remote, index) => {
                const share = () => isShareUri(remote().uri);
                const show = () => revealed().has(remote().name);
                const effectiveDefault = () =>
                  props.defaultRemote && props.defaultRemote !== "local"
                    ? props.defaultRemote
                    : "local";
                const isDefault = () => remote().name === effectiveDefault();
                const displayUri = () =>
                  share() && !show()
                    ? "share:\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022"
                    : remote().uri;
                const status = () => props.statuses?.get(remote().name) ?? null;
                const statusColor = () => {
                  const s = status();
                  return s
                    ? (STATUS_COLORS[s] ?? theme().dimFg)
                    : theme().dimFg;
                };
                const isDragOver = () => dragOverIndex() === index;

                return (
                  <div
                    role="listitem"
                    draggable={true}
                    onDragStart={(e) => handleDragStart(e, index)}
                    onDragOver={(e) => handleDragOver(e, index)}
                    onDragLeave={handleDragLeave}
                    onDrop={(e) => handleDrop(e, index)}
                    onDragEnd={handleDragEnd}
                    style={{
                      display: "flex",
                      "align-items": "stretch",
                      gap: `${scale().tightGap}px`,
                      border: `1px solid ${isDragOver() ? theme().accent : theme().subtleBorder}`,
                      "background-color": isDragOver()
                        ? theme().panelBg
                        : theme().solidPanelBg,
                      opacity: dragSourceIndex === index ? 0.5 : 1,
                      transition: "border-color 0.1s, background-color 0.1s",
                    }}
                  >
                    {/* Drag handle */}
                    <div
                      title={t("remotes.dragHandle")}
                      style={{
                        display: "flex",
                        "align-items": "center",
                        padding: `0 ${scale().controlX}px`,
                        cursor: "grab",
                        color: theme().dimFg,
                        "font-size": `${scale().md}px`,
                        "flex-shrink": 0,
                        "user-select": "none",
                        "border-right": `1px solid ${theme().subtleBorder}`,
                      }}
                    >
                      ⠿
                    </div>

                    {/* Status + Name */}
                    <div
                      style={{
                        padding: `${scale().controlY}px ${scale().controlX}px`,
                        "font-size": `${scale().md}px`,
                        "font-weight": 600,
                        "min-width": "6em",
                        "flex-shrink": 0,
                        "border-right": `1px solid ${theme().subtleBorder}`,
                        display: "flex",
                        "align-items": "center",
                        gap: `${scale().tightGap}px`,
                        overflow: "hidden",
                        "text-overflow": "ellipsis",
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

                    {/* URI (potentially masked) */}
                    <div
                      style={{
                        padding: `${scale().controlY}px ${scale().controlX}px`,
                        "font-size": `${scale().sm}px`,
                        color: share() && !show() ? theme().dimFg : theme().fg,
                        flex: 1,
                        "min-width": 0,
                        display: "flex",
                        "align-items": "center",
                        overflow: "hidden",
                        "text-overflow": "ellipsis",
                        "white-space": "nowrap",
                        "font-family":
                          share() && !show() ? "inherit" : "monospace, inherit",
                        "letter-spacing":
                          share() && !show() ? "0.05em" : "normal",
                      }}
                    >
                      {displayUri()}
                    </div>

                    {/* Default badge / Set as default button */}
                    <Show
                      when={isDefault()}
                      fallback={
                        <button
                          type="button"
                          title={t("remotes.setDefault")}
                          onClick={() => props.onSetDefault(remote().name)}
                          style={{
                            ...btnStyle(),
                            border: "none",
                            "border-left": `1px solid ${theme().subtleBorder}`,
                            opacity: 0.5,
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
                          border: "none",
                          "border-left": `1px solid ${theme().subtleBorder}`,
                          opacity: 0.7,
                          cursor: "default",
                          color: theme().accent,
                        }}
                      >
                        {t("remotes.isDefault")}
                      </div>
                    </Show>

                    {/* Reveal/hide button for share: URIs */}
                    <Show when={share()}>
                      <button
                        type="button"
                        title={
                          show() ? t("remotes.hideUri") : t("remotes.revealUri")
                        }
                        onClick={() => toggleReveal(remote().name)}
                        style={{
                          ...btnStyle(),
                          border: "none",
                          "border-left": `1px solid ${theme().subtleBorder}`,
                          opacity: 0.7,
                        }}
                      >
                        {show() ? t("remotes.hideUri") : t("remotes.revealUri")}
                      </button>
                    </Show>

                    {/* Remove button */}
                    <button
                      type="button"
                      title={t("remotes.remove")}
                      onClick={() => props.onRemove(remote().name)}
                      style={{
                        ...btnStyle(),
                        border: "none",
                        "border-left": `1px solid ${theme().subtleBorder}`,
                        opacity: 0.7,
                      }}
                    >
                      {t("remotes.remove")}
                    </button>
                  </div>
                );
              }}
            </Index>
          </div>
        </Show>

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
              ...btnStyle(),
              opacity: name().trim() && uri().trim() ? 1 : 0.4,
              "background-color": theme().accent,
              color: "#fff",
              border: `1px solid ${theme().accent}`,
            }}
          >
            {t("remotes.add")}
          </button>
        </form>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
