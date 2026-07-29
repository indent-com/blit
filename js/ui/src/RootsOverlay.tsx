/**
 * RootsOverlay — manage `blit.roots`, the declared IDE workspace roots.
 *
 * Sibling to {@link RemotesOverlay}: a drag-reorderable list of roots (name,
 * remote, path) with enable/disable + remove, and an add form. A root binds a
 * filesystem path to a declared remote (or the default target); the IDE dock
 * browses fs/git/lsp at that path on that remote. Mutations round-trip the
 * `roots-*` verbs over the config WebSocket, so they're blocked until the
 * gateway handshake completes.
 */

import { createSignal, For, Index, Show } from "solid-js";
import type {
  TerminalPalette,
  BlitWorkspace,
  ConnectionId,
} from "@blit-sh/core";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import { DirectoryPicker } from "./DirectoryPicker";
import type { Root, Remote } from "./storage";

export function RootsOverlay(props: {
  roots: Root[];
  /** Enabled remotes, to populate the add-form remote picker. */
  remotes: Remote[];
  gatewayStatus?: "connecting" | "connected" | "unavailable";
  palette: TerminalPalette;
  fontSize: number;
  /** To browse the filesystem on the selected remote. */
  workspace: BlitWorkspace;
  connectionForRemote: (remote: string) => ConnectionId;
  /** Defaults for a new root, from the active terminal. */
  defaultRemote?: string;
  defaultPath?: string;
  onAdd: (name: string, remote: string, path: string) => void;
  onRemove: (name: string) => void;
  onToggle: (name: string) => void;
  onReorder: (names: string[]) => void;
  onClose: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [name, setName] = createSignal("");
  // Prefill remote + path from the active terminal (name stays user-typed).
  const [remote, setRemote] = createSignal(props.defaultRemote ?? "");
  const [path, setPath] = createSignal(props.defaultPath ?? "");
  // When set, the form is editing this existing root (renaming removes+adds).
  const [editing, setEditing] = createSignal<string | null>(null);
  const [browsing, setBrowsing] = createSignal(false);

  function resetForm() {
    setEditing(null);
    setBrowsing(false);
    setName("");
    setRemote(props.defaultRemote ?? "");
    setPath(props.defaultPath ?? "");
  }

  function startEdit(r: Root) {
    setEditing(r.name);
    setName(r.name);
    setRemote(r.remote);
    setPath(r.path);
    nameRef?.focus();
  }

  const [dropGap, setDropGap] = createSignal<number | null>(null);
  let dragSourceIndex: number | null = null;
  let nameRef!: HTMLInputElement;

  const enabledRemotes = () => props.remotes.filter((r) => !r.disabled);

  function handleAdd(e: SubmitEvent) {
    e.preventDefault();
    const n = name().trim();
    const p = path().trim();
    // Names ride the space-delimited wire verbs and can't contain whitespace,
    // '=', or a leading '#' (the disabled marker).
    if (!n || !p || /\s/.test(n) || n.includes("=") || n.startsWith("#"))
      return;
    // Renaming an existing root: remove the old entry first. A same-name edit
    // is just a retarget (roots-add updates in place).
    const was = editing();
    if (was && was !== n) props.onRemove(was);
    props.onAdd(n, remote().trim(), p);
    resetForm();
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
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const midY = rect.top + rect.height / 2;
    setDropGap(e.clientY < midY ? index : index + 1);
  }

  function handleDrop(e: DragEvent) {
    e.preventDefault();
    const gap = dropGap();
    setDropGap(null);
    if (dragSourceIndex === null || gap === null) return;
    let insertAt = gap;
    if (insertAt > dragSourceIndex) insertAt--;
    if (insertAt === dragSourceIndex) return;
    const names = props.roots.map((r) => r.name);
    const [moved] = names.splice(dragSourceIndex, 1);
    names.splice(insertAt, 0, moved);
    props.onReorder(names);
    dragSourceIndex = null;
  }

  function handleDragEnd() {
    dragSourceIndex = null;
    setDropGap(null);
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

  // Mutations round-trip through the config WS; block while the gateway
  // handshake is incomplete. Undefined status ⇒ local-only, allow.
  const mutationsBlocked = () =>
    props.gatewayStatus !== undefined && props.gatewayStatus !== "connected";

  // Columns: drag, name, remote:path, edit, toggle, remove.
  const cols = () => "auto auto 1fr auto auto auto";

  const spec = (r: Root) => (r.remote ? `${r.remote}:${r.path}` : r.path);

  return (
    <OverlayBackdrop
      palette={props.palette}
      label="Workspace roots"
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
          "min-width": "min(90vw, 34em)",
        }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title="Workspace roots"
          onClose={props.onClose}
        />

        <Show
          when={props.roots.length > 0}
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
                No workspace roots yet.
              </div>
              <Show when={!mutationsBlocked()}>
                <div>Add a name and path below to browse it in the dock.</div>
              </Show>
            </div>
          }
        >
          <div
            role="list"
            onDragOver={(e) => {
              e.preventDefault();
              if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
            }}
            onDrop={handleDrop}
            onDragLeave={(e) => {
              const related = e.relatedTarget as Node | null;
              if (!related || !e.currentTarget.contains(related)) {
                setDropGap(null);
              }
            }}
            style={{
              display: "grid",
              "grid-template-columns": cols(),
              "max-height": "60vh",
              "overflow-y": "auto",
              ...scrollbarStyle(theme()),
            }}
          >
            <Index each={props.roots}>
              {(root, index) => {
                const disabled = () => root().disabled;
                const rowOpacity = () => (dragSourceIndex === index ? 0.5 : 1);
                const isActiveGap = (gap: number) =>
                  dragSourceIndex !== null &&
                  gap !== dragSourceIndex &&
                  gap !== dragSourceIndex + 1;
                const showGapBefore = () => {
                  const gap = dropGap();
                  return gap !== null && gap === index && isActiveGap(gap);
                };
                const showGapAfter = () => {
                  const gap = dropGap();
                  return (
                    gap !== null &&
                    gap === index + 1 &&
                    index === props.roots.length - 1 &&
                    isActiveGap(gap)
                  );
                };

                return (
                  <div
                    role="listitem"
                    draggable={!mutationsBlocked()}
                    onDragStart={(e) => handleDragStart(e, index)}
                    onDragOver={(e) => handleDragOver(e, index)}
                    onDrop={handleDrop}
                    onDragEnd={handleDragEnd}
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
                    <div
                      title="Drag to reorder"
                      style={{
                        display: "flex",
                        "align-items": "center",
                        "align-self": "stretch",
                        "justify-content": "center",
                        padding: `0 ${scale().controlX}px`,
                        cursor: mutationsBlocked() ? "not-allowed" : "grab",
                        color: theme().dimFg,
                        "font-size": `${scale().md}px`,
                        "user-select": "none",
                        "border-right": `1px solid ${theme().subtleBorder}`,
                        opacity: mutationsBlocked() ? 0.4 : 1,
                      }}
                    >
                      ⠿
                    </div>

                    {/* Name */}
                    <div
                      style={{
                        padding: `${scale().controlY}px ${scale().controlX}px`,
                        "font-size": `${scale().md}px`,
                        "font-weight": 600,
                        "white-space": "nowrap",
                      }}
                    >
                      {root().name}
                    </div>

                    {/* remote:path */}
                    <div
                      style={{
                        padding: `${scale().controlY}px ${scale().controlX}px`,
                        "font-size": `${scale().sm}px`,
                        color: theme().fg,
                        overflow: "hidden",
                        "text-overflow": "ellipsis",
                        "white-space": "nowrap",
                        "font-family": "monospace, inherit",
                      }}
                      title={spec(root())}
                    >
                      {spec(root())}
                    </div>

                    {/* Edit */}
                    <button
                      type="button"
                      title="Edit"
                      disabled={mutationsBlocked()}
                      onClick={() => startEdit(root())}
                      style={{
                        ...btnStyle(),
                        opacity: mutationsBlocked() ? 0.3 : 0.7,
                        cursor: mutationsBlocked() ? "not-allowed" : "pointer",
                        "border-left": `1px solid ${theme().subtleBorder}`,
                        color:
                          editing() === root().name
                            ? theme().accent
                            : "inherit",
                      }}
                    >
                      Edit
                    </button>

                    {/* Disable / Enable */}
                    <button
                      type="button"
                      title={disabled() ? "Enable" : "Disable"}
                      disabled={mutationsBlocked()}
                      onClick={() => props.onToggle(root().name)}
                      style={{
                        ...btnStyle(),
                        opacity: mutationsBlocked() ? 0.3 : 0.7,
                        cursor: mutationsBlocked() ? "not-allowed" : "pointer",
                        "border-left": `1px solid ${theme().subtleBorder}`,
                      }}
                    >
                      {disabled() ? "Enable" : "Disable"}
                    </button>

                    {/* Remove */}
                    <button
                      type="button"
                      title="Remove"
                      disabled={mutationsBlocked()}
                      onClick={() => props.onRemove(root().name)}
                      style={{
                        ...btnStyle(),
                        opacity: mutationsBlocked() ? 0.3 : 0.7,
                        cursor: mutationsBlocked() ? "not-allowed" : "pointer",
                      }}
                    >
                      Remove
                    </button>
                  </div>
                );
              }}
            </Index>
          </div>
        </Show>

        <Show when={!mutationsBlocked()}>
          {/* Add form: name, remote (optional), path */}
          <form
            onSubmit={handleAdd}
            style={{
              display: "flex",
              gap: `${scale().tightGap}px`,
              "align-items": "stretch",
              "border-top": `1px solid ${theme().subtleBorder}`,
              "padding-top": `${scale().gap}px`,
              "flex-wrap": "wrap",
            }}
          >
            <input
              ref={nameRef}
              name="blit-root-name"
              type="text"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
              placeholder="name"
              autocomplete="off"
              autocorrect="off"
              autocapitalize="off"
              spellcheck={false}
              style={{ ...inputStyle(), flex: "0 0 8em", "font-weight": 600 }}
            />
            <select
              value={remote()}
              onChange={(e) => setRemote(e.currentTarget.value)}
              title="Remote (blank = default target)"
              style={{
                ...ui.input,
                "background-color": theme().inputBg,
                color: "inherit",
                "font-size": `${scale().md}px`,
                "border-radius": "0",
                flex: "0 0 auto",
              }}
            >
              <option value="">(default)</option>
              <For each={enabledRemotes()}>
                {(r) => <option value={r.name}>{r.name}</option>}
              </For>
            </select>
            <input
              name="blit-root-path"
              type="text"
              value={path()}
              onInput={(e) => setPath(e.currentTarget.value)}
              placeholder="/absolute/path"
              autocomplete="off"
              autocorrect="off"
              autocapitalize="off"
              spellcheck={false}
              style={{ ...inputStyle(), "font-family": "monospace, inherit" }}
            />
            <button
              type="button"
              onClick={() => setBrowsing((v) => !v)}
              title="Browse the filesystem"
              style={{
                ...btnStyle(),
                "flex-shrink": 0,
                border: `1px solid ${theme().subtleBorder}`,
                opacity: browsing() ? 1 : 0.7,
              }}
            >
              Browse…
            </button>
            <button
              type="submit"
              disabled={!name().trim() || !path().trim()}
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
                opacity: name().trim() && path().trim() ? 1 : 0.4,
              }}
            >
              {editing() ? "Save" : "Add"}
            </button>
            <Show when={editing()}>
              <button
                type="button"
                onClick={resetForm}
                style={{
                  ...btnStyle(),
                  "flex-shrink": 0,
                  border: `1px solid ${theme().subtleBorder}`,
                }}
              >
                Cancel
              </button>
            </Show>
          </form>

          <Show when={browsing()}>
            <DirectoryPicker
              workspace={props.workspace}
              connectionId={props.connectionForRemote(remote().trim())}
              initialPath={path().trim() || props.defaultPath || "/"}
              theme={theme()}
              scale={scale()}
              fontFamily="inherit"
              fontSize={props.fontSize}
              onPick={(p) => {
                setPath(p);
                setBrowsing(false);
              }}
              onCancel={() => setBrowsing(false)}
            />
          </Show>
        </Show>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
