/**
 * The journal half of the systemd panel: one page at a time, anchored by
 * cursor.
 *
 * A journal is far too large to mirror, so unlike the unit table nothing here
 * is live state — every view is a query, and paging is the journal's own
 * cursors rather than an offset that would drift as entries arrive. Filtering
 * and search run in `journalctl`, so a search covers the whole boot instead of
 * whatever happened to be fetched.
 */

import { createSignal, For, onMount, Show } from "solid-js";
import type { TerminalPalette } from "@blit-sh/core";
import type {
  SystemdBoot,
  SystemdLogEntry,
  SystemdLogQuery,
  SystemdUnitsHandle,
} from "./systemd";
import { mergeStyle, scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import { t, tp } from "./i18n";

const PAGE = 200;

/** syslog severities, for the priority filter and the colour of a row. */
const SEVERITY = [
  "emerg",
  "alert",
  "crit",
  "err",
  "warning",
  "notice",
  "info",
  "debug",
];

function formatTimestamp(realtime: string): string {
  const micros = Number(realtime);
  if (!Number.isFinite(micros) || micros <= 0) return "";
  const date = new Date(micros / 1000);
  const pad = (value: number, width = 2) => String(value).padStart(width, "0");
  return (
    `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}.` +
    `${pad(date.getMilliseconds(), 3)}`
  );
}

export function SystemdLogs(props: {
  handle: SystemdUnitsHandle | null;
  palette: TerminalPalette;
  fontSize: number;
  /** Prefilled when the panel was opened from a unit row. */
  initialUnit?: string;
  initialScope?: string;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [scope, setScope] = createSignal(props.initialScope ?? "system");
  const [unit, setUnit] = createSignal(props.initialUnit ?? "");
  const [boot, setBoot] = createSignal("");
  const [priority, setPriority] = createSignal("");
  const [grep, setGrep] = createSignal("");
  const [entries, setEntries] = createSignal<readonly SystemdLogEntry[]>([]);
  const [boots, setBoots] = createSignal<readonly SystemdBoot[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [olderLeft, setOlderLeft] = createSignal(true);

  let list: HTMLDivElement | undefined;

  const filters = (): SystemdLogQuery => ({
    scope: scope() as SystemdLogQuery["scope"],
    unit: unit().trim() || undefined,
    boot: boot() || undefined,
    priority: priority() || undefined,
    grep: grep().trim() || undefined,
    limit: PAGE,
  });

  const run = async (
    query: SystemdLogQuery,
    merge: (page: readonly SystemdLogEntry[]) => readonly SystemdLogEntry[],
  ) => {
    const handle = props.handle;
    if (!handle || busy()) return;
    setBusy(true);
    setError(null);
    try {
      const page = await handle.logs(query);
      setEntries(merge(page.entries));
      if (query.direction === "backward" && query.cursor) {
        setOlderLeft(page.more);
      } else if (!query.cursor) {
        setOlderLeft(page.more);
      }
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  /** Newest page, scrolled to the bottom the way a log is read. */
  const reload = async () => {
    await run({ ...filters() }, (page) => page);
    queueMicrotask(() => {
      if (list) list.scrollTop = list.scrollHeight;
    });
  };

  const older = async () => {
    const first = entries()[0];
    if (!first) return reload();
    const anchored = list?.scrollHeight ?? 0;
    await run(
      { ...filters(), cursor: first.cursor, direction: "backward" },
      (page) => [...page, ...entries()],
    );
    // Keep the row the reader was looking at under the same pixel.
    queueMicrotask(() => {
      if (list) list.scrollTop = list.scrollHeight - anchored;
    });
  };

  const newer = async () => {
    const last = entries().at(-1);
    if (!last) return reload();
    await run(
      { ...filters(), cursor: last.cursor, direction: "forward" },
      (page) => [...entries(), ...page],
    );
  };

  onMount(() => {
    void reload();
    void props.handle
      ?.boots()
      .then(setBoots)
      .catch(() => setBoots([]));
  });

  const severityColor = (priorityValue: string): string => {
    const level = Number(priorityValue);
    if (!Number.isFinite(level)) return theme().fg;
    if (level <= 3) return theme().error;
    if (level === 4) return theme().warning;
    if (level >= 7) return theme().dimFg;
    return theme().fg;
  };

  const control = (): Record<string, string> => ({
    "font-size": `${scale().sm}px`,
  });

  return (
    <div
      style={{
        display: "flex",
        "flex-direction": "column",
        gap: `${scale().xs}px`,
      }}
    >
      <div
        style={{
          display: "flex",
          gap: `${scale().xs}px`,
          "align-items": "center",
          "flex-wrap": "wrap",
        }}
      >
        <select
          value={scope()}
          onChange={(event) => {
            setScope(event.currentTarget.value);
            void reload();
          }}
          style={mergeStyle(ui.input, control())}
        >
          <option value="system">{t("systemd.scopeSystem")}</option>
          <option value="user">{t("systemd.scopeUser")}</option>
          <option value="all">{t("systemd.scopeAll")}</option>
        </select>
        <input
          value={unit()}
          placeholder={t("systemd.logsUnit")}
          onInput={(event) => setUnit(event.currentTarget.value)}
          onChange={() => void reload()}
          style={mergeStyle(ui.input, { ...control(), width: "16em" })}
        />
        <select
          value={boot()}
          onChange={(event) => {
            setBoot(event.currentTarget.value);
            void reload();
          }}
          style={mergeStyle(ui.input, control())}
        >
          <option value="">{t("systemd.bootAny")}</option>
          <For each={boots()}>
            {(entry) => (
              <option value={entry.boot}>
                {tp("systemd.bootLabel", {
                  index: entry.index,
                  id: entry.boot.slice(0, 8),
                })}
              </option>
            )}
          </For>
        </select>
        <select
          value={priority()}
          onChange={(event) => {
            setPriority(event.currentTarget.value);
            void reload();
          }}
          style={mergeStyle(ui.input, control())}
        >
          <option value="">{t("systemd.priorityAny")}</option>
          <For each={SEVERITY}>
            {(name, index) => <option value={String(index())}>{name}</option>}
          </For>
        </select>
        <input
          value={grep()}
          placeholder={t("systemd.logsSearch")}
          onInput={(event) => setGrep(event.currentTarget.value)}
          onChange={() => void reload()}
          style={mergeStyle(ui.input, { ...control(), flex: "1 1 12em" })}
        />
        <button
          type="button"
          style={mergeStyle(ui.btn, control())}
          onClick={() => void reload()}
        >
          {t("systemd.logsRefresh")}
        </button>
      </div>

      <Show when={error()}>
        <div style={{ color: theme().error, "font-size": `${scale().sm}px` }}>
          {error()}
        </div>
      </Show>

      <div
        style={{
          display: "flex",
          gap: `${scale().sm}px`,
          "align-items": "center",
        }}
      >
        <button
          type="button"
          disabled={busy() || !olderLeft()}
          style={mergeStyle(ui.btn, control())}
          onClick={() => void older()}
        >
          {t("systemd.logsOlder")}
        </button>
        <button
          type="button"
          disabled={busy()}
          style={mergeStyle(ui.btn, control())}
          onClick={() => void newer()}
        >
          {t("systemd.logsNewer")}
        </button>
        <span style={{ color: theme().dimFg, "font-size": `${scale().sm}px` }}>
          {busy()
            ? t("systemd.logsLoading")
            : tp("systemd.logsCount", { count: String(entries().length) })}
        </span>
      </div>

      <div
        ref={list}
        style={mergeStyle(scrollbarStyle(theme()), {
          "overflow-y": "auto",
          "max-height": "56vh",
          "font-size": `${scale().sm}px`,
          "line-height": "1.45",
        })}
      >
        <For
          each={entries()}
          fallback={
            <div style={{ color: theme().dimFg, padding: `${scale().sm}px 0` }}>
              {busy() ? t("systemd.logsLoading") : t("systemd.logsEmpty")}
            </div>
          }
        >
          {(entry) => (
            <div
              style={{
                display: "grid",
                "grid-template-columns": "12em 16em minmax(0, 1fr)",
                gap: `${scale().sm}px`,
                padding: `1px 0`,
              }}
            >
              <span style={{ color: theme().dimFg }}>
                {formatTimestamp(entry.realtime)}
              </span>
              <span
                style={{
                  color: theme().dimFg,
                  overflow: "hidden",
                  "text-overflow": "ellipsis",
                  "white-space": "nowrap",
                }}
                title={entry.unit}
              >
                {entry.unit}
                {entry.pid ? `[${entry.pid}]` : ""}
              </span>
              <span
                style={{
                  color: severityColor(entry.priority),
                  "white-space": "pre-wrap",
                  "word-break": "break-word",
                }}
              >
                {entry.message}
              </span>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}
