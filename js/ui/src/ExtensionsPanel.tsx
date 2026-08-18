/**
 * What this server is running, and what it could run.
 *
 * Two lists over the same identity: an extension is its BLAKE3 digest, so the
 * installed table shows the digest a definition is pinned to and the registry
 * shows the digest it offers. Comparing the two is what "up to date" means
 * here — there is no version to trust.
 */

import { createSignal, For, onMount, Show } from "solid-js";
import type {
  BlitExtensionRecord,
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import {
  EXT_CONTROL_DISABLE,
  EXT_CONTROL_REMOVE,
  EXT_CONTROL_RESTART,
  EXT_FLAG_ENABLED,
  EXT_FLAG_PERSIST,
  EXT_PHASE_NAMES,
  EXT_PHASE_RUNNING,
  formatExtensionId,
} from "@blit-sh/core";
import {
  defaultRegistry,
  fetchRegistry,
  installFromRegistry,
  type Registry,
  type RegistryEntry,
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

  const install = (entry: RegistryEntry) => {
    const connection = host();
    const source = registry();
    if (!connection || !source) return;
    const existing = installed().find((record) => record.name === entry.name);
    void act(entry.name, async () => {
      await installFromRegistry(connection, source, entry, existing);
      setNote(tp("extensions.installed", { name: entry.name }));
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

  const short = (digest: string) => digest.slice(0, 12);

  const upToDate = (entry: RegistryEntry): BlitExtensionRecord | undefined =>
    installed().find(
      (record) => record.name === entry.name && record.hash === entry.blake3,
    );

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
          color: theme().dimFg,
          "font-size": `${scale().sm}px`,
          "margin-bottom": `${scale().xs}px`,
        }}
      >
        {t("extensions.installedTitle")}
      </div>
      <div
        style={mergeStyle(scrollbarStyle(theme()), {
          "overflow-y": "auto",
          "max-height": "30vh",
          "font-size": `${scale().sm}px`,
          "margin-bottom": `${scale().sm}px`,
        })}
      >
        <For
          each={installed()}
          fallback={
            <div style={{ color: theme().dimFg }}>{t("extensions.none")}</div>
          }
        >
          {(record) => (
            <div
              style={{
                display: "grid",
                "grid-template-columns":
                  "minmax(0, 1fr) 7em 9em 8em minmax(0, 12em)",
                gap: `${scale().sm}px`,
                "align-items": "center",
                padding: `${scale().xs}px 0`,
                "border-bottom": `1px solid ${theme().border}`,
              }}
            >
              <span title={`id:${formatExtensionId(record.extensionId)}`}>
                {record.name || `id:${formatExtensionId(record.extensionId)}`}
              </span>
              <span
                style={{
                  color:
                    record.phase === EXT_PHASE_RUNNING
                      ? theme().success
                      : theme().dimFg,
                }}
              >
                {EXT_PHASE_NAMES[record.phase] ?? record.phase}
              </span>
              <span style={{ color: theme().dimFg }} title={record.hash}>
                {short(record.hash)}
              </span>
              <span style={{ color: theme().dimFg }}>
                {record.flags & EXT_FLAG_PERSIST
                  ? t("extensions.persistent")
                  : t("extensions.transient")}
                {record.flags & EXT_FLAG_ENABLED
                  ? ""
                  : ` ${t("extensions.disabled")}`}
              </span>
              <span style={{ display: "flex", gap: `${scale().xs}px` }}>
                <button
                  type="button"
                  disabled={busy() !== null}
                  style={mergeStyle(ui.btn, {
                    "font-size": `${scale().sm}px`,
                  })}
                  onClick={() =>
                    void act(record.name, () =>
                      host()!.controlExtension(
                        record.extensionId,
                        EXT_CONTROL_RESTART,
                      ),
                    )
                  }
                >
                  {t("extensions.restart")}
                </button>
                <button
                  type="button"
                  disabled={busy() !== null}
                  style={mergeStyle(ui.btn, {
                    "font-size": `${scale().sm}px`,
                  })}
                  onClick={() => remove(record)}
                >
                  {t("extensions.remove")}
                </button>
              </span>
            </div>
          )}
        </For>
      </div>

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
          "max-height": "30vh",
          "font-size": `${scale().sm}px`,
        })}
      >
        <For
          each={registry()?.extensions ?? []}
          fallback={
            <div style={{ color: theme().dimFg }}>
              {busy() === "registry"
                ? t("extensions.loading")
                : t("extensions.registryEmpty")}
            </div>
          }
        >
          {(entry) => (
            <div
              style={{
                display: "grid",
                "grid-template-columns": "minmax(0, 1fr) 9em 7em 8em",
                gap: `${scale().sm}px`,
                "align-items": "center",
                padding: `${scale().xs}px 0`,
                "border-bottom": `1px solid ${theme().border}`,
              }}
            >
              <span>{entry.name}</span>
              <span style={{ color: theme().dimFg }} title={entry.blake3}>
                {short(entry.blake3)}
              </span>
              <span style={{ color: theme().dimFg }}>
                {entry.brotliBytes
                  ? `${Math.round(entry.brotliBytes / 1024)} KiB`
                  : ""}
              </span>
              <button
                type="button"
                disabled={busy() !== null || upToDate(entry) !== undefined}
                style={mergeStyle(ui.btn, { "font-size": `${scale().sm}px` })}
                onClick={() => install(entry)}
              >
                {upToDate(entry)
                  ? t("extensions.current")
                  : installed().some((record) => record.name === entry.name)
                    ? t("extensions.update")
                    : t("extensions.install")}
              </button>
            </div>
          )}
        </For>
      </div>
    </>
  );
}
