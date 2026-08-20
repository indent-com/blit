/**
 * What this server is running, and what it could run.
 *
 * One list over one identity: an extension is its BLAKE3 digest, so a row that
 * is both installed and offered shows the digest the definition is pinned to
 * next to the one the registry offers, and "outdated" is that comparison. There
 * is no version to trust. Installed and offered used to be two tables, which
 * named the same extension twice and made an update look like a fresh install.
 */

import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import type {
  BlitExtensionRecord,
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import {
  EXT_CONTROL_CANCEL,
  EXT_CONTROL_DISABLE,
  EXT_CONTROL_ENABLE,
  EXT_CONTROL_REMOVE,
  EXT_CONTROL_RESTART,
  EXT_FLAG_ENABLED,
  EXT_FLAG_PERSIST,
  EXT_PHASE_BLOCKED,
  EXT_PHASE_NAMES,
  EXT_PHASE_RUNNING,
  EXT_PHASE_STOPPED,
  formatExtensionId,
} from "@blit-sh/core";
import {
  defaultRegistry,
  fetchRegistry,
  installFromRegistry,
  isOutdated,
  mergeExtensions,
  type ExtensionRow,
  type Registry,
} from "./extensionRegistry";
import { mergeStyle, scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import { t, tp } from "./i18n";

export function ExtensionsPanel(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  palette: TerminalPalette;
  fontSize: number;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [installed, setInstalled] = createSignal<BlitExtensionRecord[]>([]);
  const [registry, setRegistry] = createSignal<Registry | null>(null);
  const [registryUrl, setRegistryUrl] = createSignal(defaultRegistry());
  const [error, setError] = createSignal<string | null>(null);
  const [note, setNote] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal<string | null>(null);

  const host = () => props.workspace.getConnection(props.connectionId);

  const refresh = async () => {
    const connection = host();
    if (!connection) return;
    try {
      setInstalled(await connection.listExtensions());
      setError(null);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    }
  };

  const loadRegistry = async () => {
    setBusy("registry");
    try {
      setRegistry(await fetchRegistry(registryUrl()));
      setError(null);
    } catch (failure) {
      setRegistry(null);
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(null);
    }
  };

  onMount(() => {
    void refresh();
    void loadRegistry();
  });

  const rows = createMemo<ExtensionRow[]>(() =>
    mergeExtensions(installed(), registry()?.extensions ?? []),
  );

  const act = async (label: string, action: () => Promise<unknown>) => {
    setBusy(label);
    setNote(null);
    try {
      await action();
      setError(null);
      await refresh();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(null);
    }
  };

  /**
   * Install, or replace the definition of the same name in place.
   *
   * The installed record is what makes the second case an update rather than a
   * second definition: it carries the CAS token of what is being replaced.
   */
  const install = (row: ExtensionRow) => {
    const connection = host();
    const source = registry();
    if (!connection || !source || !row.offered) return;
    void act(row.label, async () => {
      await installFromRegistry(
        connection,
        source,
        row.offered!,
        row.installed,
      );
      setNote(tp("extensions.installed", { name: row.label }));
    });
  };

  /** Removal is a two-step verb: a definition must be quiescent first. */
  const remove = (record: BlitExtensionRecord) => {
    const connection = host();
    if (!connection) return;
    void act(record.name, async () => {
      await connection.controlExtension(
        record.extensionId,
        EXT_CONTROL_DISABLE,
      );
      await connection.controlExtension(record.extensionId, EXT_CONTROL_REMOVE);
      setNote(tp("extensions.removed", { name: record.name }));
    });
  };

  const control = (
    record: BlitExtensionRecord,
    action: number,
    noteKey: string,
  ) => {
    const connection = host();
    if (!connection) return;
    void act(record.name, async () => {
      await connection.controlExtension(record.extensionId, action);
      setNote(tp(noteKey, { name: record.name }));
    });
  };

  const short = (digest: string) => digest.slice(0, 12);
  const isStopped = (record: BlitExtensionRecord) =>
    record.phase === EXT_PHASE_STOPPED || record.phase === EXT_PHASE_BLOCKED;
  const isPersistent = (record: BlitExtensionRecord) =>
    (record.flags & EXT_FLAG_PERSIST) !== 0;
  const isEnabled = (record: BlitExtensionRecord) =>
    (record.flags & EXT_FLAG_ENABLED) !== 0;

  return (
    <>
      <Show when={error()}>
        <div
          style={{
            color: theme().error,
            "font-size": `${scale().sm}px`,
            "margin-bottom": `${scale().xs}px`,
          }}
        >
          {error()}
        </div>
      </Show>
      <Show when={note()}>
        <div
          style={{
            color: theme().dimFg,
            "font-size": `${scale().sm}px`,
            "margin-bottom": `${scale().xs}px`,
          }}
        >
          {note()}
        </div>
      </Show>

      <div
        style={{
          display: "flex",
          gap: `${scale().xs}px`,
          "align-items": "center",
          "margin-bottom": `${scale().xs}px`,
        }}
      >
        <span style={{ color: theme().dimFg, "font-size": `${scale().sm}px` }}>
          {t("extensions.registryTitle")}
        </span>
        <input
          data-registry-url
          value={registryUrl()}
          onInput={(event) => setRegistryUrl(event.currentTarget.value)}
          onChange={() => void loadRegistry()}
          style={mergeStyle(ui.input, {
            flex: "1 1 auto",
            "font-size": `${scale().sm}px`,
          })}
        />
        <button
          type="button"
          disabled={busy() !== null}
          style={mergeStyle(ui.btn, { "font-size": `${scale().sm}px` })}
          onClick={() => void loadRegistry()}
        >
          {t("extensions.reload")}
        </button>
      </div>

      <div
        style={mergeStyle(scrollbarStyle(theme()), {
          "overflow-y": "auto",
          // Bounded by the pane rather than the viewport — see SystemdPanel's
          // table for why a `vh` cap inside a pane scrolls twice.
          flex: "1 1 0",
          "min-height": "6em",
          "font-size": `${scale().sm}px`,
        })}
      >
        <For
          each={rows()}
          fallback={
            <div style={{ color: theme().dimFg }}>
              {busy() === "registry"
                ? t("extensions.loading")
                : t("extensions.none")}
            </div>
          }
        >
          {(row) => (
            <div
              data-extension={row.label}
              style={{
                display: "flex",
                "flex-direction": "column",
                gap: `${scale().xs}px`,
                padding: `${scale().xs}px 0`,
                "border-bottom": `1px solid ${theme().border}`,
              }}
            >
              <div
                style={{
                  display: "grid",
                  // The info line keeps fixed columns so phase/digest/flags line
                  // up across rows. Actions live on their own line below.
                  "grid-template-columns": "minmax(0, 1fr) 6em 13em 7em",
                  gap: `${scale().sm}px`,
                  "align-items": "center",
                }}
              >
                <span style={{ "min-width": 0 }}>
                  <span
                    title={
                      row.installed
                        ? `id:${formatExtensionId(row.installed.extensionId)}`
                        : undefined
                    }
                  >
                    {row.label}
                  </span>
                  <Show when={row.description}>
                    <div
                      style={{
                        color: theme().dimFg,
                        "font-size": `${scale().xs}px`,
                      }}
                    >
                      {row.description}
                    </div>
                  </Show>
                </span>

                <span
                  style={{
                    color: !row.installed
                      ? theme().dimFg
                      : row.installed.phase === EXT_PHASE_RUNNING
                        ? theme().success
                        : theme().dimFg,
                  }}
                >
                  {row.installed
                    ? (EXT_PHASE_NAMES[row.installed.phase] ??
                      row.installed.phase)
                    : t("extensions.available")}
                </span>

                {/* The digest is the identity, so an update is shown as one. */}
                <span
                  style={{
                    color: isOutdated(row) ? theme().warning : theme().dimFg,
                  }}
                  title={
                    row.installed && row.offered
                      ? `${row.installed.hash}\n${row.offered.blake3}`
                      : (row.installed?.hash ?? row.offered?.blake3)
                  }
                >
                  {short(row.installed?.hash ?? row.offered?.blake3 ?? "")}
                  <Show when={isOutdated(row)}>
                    {" → "}
                    {short(row.offered!.blake3)}
                  </Show>
                </span>

                <span style={{ color: theme().dimFg }}>
                  <Show
                    when={row.installed}
                    fallback={
                      row.offered?.brotliBytes
                        ? `${Math.round(row.offered.brotliBytes / 1024)} KiB`
                        : ""
                    }
                  >
                    {row.installed!.flags & EXT_FLAG_PERSIST
                      ? t("extensions.persistent")
                      : t("extensions.transient")}
                    {row.installed!.flags & EXT_FLAG_ENABLED
                      ? ""
                      : ` ${t("extensions.disabled")}`}
                  </Show>
                </span>
              </div>

              <div
                style={{
                  display: "flex",
                  gap: `${scale().xs}px`,
                  "align-items": "center",
                  "justify-content": "flex-end",
                  "flex-wrap": "wrap",
                }}
              >
                <Show when={row.offered && !row.installed}>
                  <button
                    type="button"
                    disabled={busy() !== null}
                    style={mergeStyle(ui.btn, {
                      "font-size": `${scale().sm}px`,
                    })}
                    onClick={() => install(row)}
                  >
                    {t("extensions.install")}
                  </button>
                </Show>
                <Show when={isOutdated(row)}>
                  <button
                    type="button"
                    data-extension-update
                    disabled={busy() !== null}
                    style={mergeStyle(ui.btn, {
                      "font-size": `${scale().sm}px`,
                    })}
                    onClick={() => install(row)}
                  >
                    {t("extensions.update")}
                  </button>
                </Show>
                <Show when={row.installed && row.offered && !isOutdated(row)}>
                  <span style={{ color: theme().dimFg }}>
                    {t("extensions.current")}
                  </span>
                </Show>
                <Show when={row.installed}>
                  <Show when={isEnabled(row.installed!)}>
                    <button
                      type="button"
                      disabled={busy() !== null}
                      style={mergeStyle(ui.btn, {
                        "font-size": `${scale().sm}px`,
                      })}
                      onClick={() =>
                        control(
                          row.installed!,
                          EXT_CONTROL_RESTART,
                          isStopped(row.installed!)
                            ? "extensions.started"
                            : "extensions.restarted",
                        )
                      }
                    >
                      {isStopped(row.installed!)
                        ? t("extensions.start")
                        : t("extensions.restart")}
                    </button>
                  </Show>
                  <Show when={!isStopped(row.installed!)}>
                    <button
                      type="button"
                      disabled={busy() !== null}
                      style={mergeStyle(ui.btn, {
                        "font-size": `${scale().sm}px`,
                      })}
                      onClick={() =>
                        control(row.installed!, EXT_CONTROL_CANCEL, "extensions.stopped")
                      }
                    >
                      {t("extensions.stop")}
                    </button>
                  </Show>
                  <Show when={isPersistent(row.installed!)}>
                    <Show
                      when={isEnabled(row.installed!)}
                      fallback={
                        <button
                          type="button"
                          disabled={busy() !== null}
                          style={mergeStyle(ui.btn, {
                            "font-size": `${scale().sm}px`,
                          })}
                          onClick={() =>
                            control(
                              row.installed!,
                              EXT_CONTROL_ENABLE,
                              "extensions.enabledNote",
                            )
                          }
                        >
                          {t("extensions.enable")}
                        </button>
                      }
                    >
                      <button
                        type="button"
                        disabled={busy() !== null}
                        style={mergeStyle(ui.btn, {
                          "font-size": `${scale().sm}px`,
                        })}
                        onClick={() =>
                          control(
                            row.installed!,
                            EXT_CONTROL_DISABLE,
                            "extensions.disabledNote",
                          )
                        }
                      >
                        {t("extensions.disable")}
                      </button>
                    </Show>
                    <button
                      type="button"
                      disabled={busy() !== null}
                      style={mergeStyle(ui.btn, {
                        "font-size": `${scale().sm}px`,
                      })}
                      onClick={() => remove(row.installed!)}
                    >
                      {t("extensions.remove")}
                    </button>
                  </Show>
                </Show>
              </div>
            </div>
          )}
        </For>
      </div>
    </>
  );
}
