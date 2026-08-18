/**
 * The journal half of the systemd panel: a live tail over a page that grows as
 * it is scrolled, both anchored by cursor.
 *
 * A journal is far too large to mirror, so unlike the unit table nothing here
 * is a snapshot of everything — history is paged in from the journal's own
 * cursors rather than an offset that would drift as entries arrive, and the
 * present is a `journalctl --follow` resumed from the cursor the loaded page
 * ends on, which is what makes the join seamless. Filtering and search run in
 * `journalctl`, so a search covers the whole boot instead of whatever happened
 * to be fetched.
 */

import { createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { TerminalPalette } from "@blit-sh/core";
import type {
  SystemdBoot,
  SystemdLogEntry,
  SystemdLogFollow,
  SystemdLogQuery,
  SystemdUnitsHandle,
} from "./systemd";
import { mergeStyle, scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import { t, tp } from "./i18n";

const PAGE = 200;

/**
 * Entries a running tail will hold.
 *
 * Only a tail trims, and only while the reader is at the bottom: scrolled-back
 * history is what the reader asked for and must not evaporate under them,
 * whereas a pane left tailing a busy unit overnight is a leak nobody asked for.
 */
const MAX_ENTRIES = 5000;

/** Distance from an edge that counts as being at it. */
const EDGE = 64;

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
  const [newerLeft, setNewerLeft] = createSignal(true);
  const [live, setLive] = createSignal(true);

  let list: HTMLDivElement | undefined;
  let tail: SystemdLogFollow | null = null;

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
  ): Promise<readonly SystemdLogEntry[] | null> => {
    const handle = props.handle;
    if (!handle || busy()) return null;
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
      return page.entries;
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
      return null;
    } finally {
      setBusy(false);
    }
  };

  /** Whether the reader is parked at the newest row rather than reading back. */
  const atBottom = () =>
    !list || list.scrollHeight - list.scrollTop - list.clientHeight <= EDGE;

  const toBottom = () =>
    queueMicrotask(() => {
      if (list) list.scrollTop = list.scrollHeight;
    });

  /**
   * A tail is the boot doing the writing, so a past boot cannot have one — and
   * a filter narrow enough to be worth reading is still worth tailing.
   */
  const canFollow = () => {
    const chosen = boot();
    if (!chosen) return true;
    return boots().find((entry) => entry.boot === chosen)?.index === "0";
  };

  /** Whether entries are arriving on their own right now. */
  const tailing = () => live() && canFollow();

  const stopTail = () => {
    tail?.close();
    tail = null;
  };

  /** Resume the live tail from the newest entry held, so the join has no gap. */
  const startTail = () => {
    stopTail();
    const handle = props.handle;
    if (!handle || !live() || !canFollow()) return;
    tail = handle.followLogs(
      { ...filters(), cursor: entries().at(-1)?.cursor },
      {
        onEntries: (page) => {
          if (!page.length) return;
          const pinned = atBottom();
          setEntries((current) => {
            const merged = [...current, ...page];
            return pinned && merged.length > MAX_ENTRIES
              ? merged.slice(merged.length - MAX_ENTRIES)
              : merged;
          });
          setNewerLeft(true);
          if (pinned) toBottom();
        },
        onEnd: (message) => {
          // Whatever ended it, the pane is no longer live, and a pane that
          // looks live while standing still is worse than one that says so.
          tail = null;
          setLive(false);
          if (message) setError(message);
        },
      },
    );
  };

  /** Newest page, scrolled to the bottom the way a log is read. */
  const reload = async () => {
    stopTail();
    setNewerLeft(true);
    await run({ ...filters() }, (page) => page);
    toBottom();
    startTail();
  };

  const older = async () => {
    const first = entries()[0];
    if (!first) return reload();
    if (!olderLeft() || busy()) return;
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
    if (busy()) return;
    const page = await run(
      { ...filters(), cursor: last.cursor, direction: "forward" },
      (rows) => [...entries(), ...rows],
    );
    // Nothing newer to read: stop asking until something moves the end again.
    if (page && page.length === 0) setNewerLeft(false);
  };

  /**
   * Paging as scrolling, in the direction the reader is heading.
   *
   * Reaching the top always loads history. Reaching the bottom only pages
   * forward when nothing is tailing — a live tail already owns that edge, and
   * asking for a page it is about to deliver would double every entry.
   */
  const onScroll = () => {
    if (!list || busy()) return;
    if (list.scrollTop <= EDGE) {
      void older();
      return;
    }
    if (tailing() || !newerLeft()) return;
    if (list.scrollHeight - list.scrollTop - list.clientHeight <= EDGE) {
      void newer();
    }
  };

  onMount(() => {
    void reload();
    void props.handle
      ?.boots()
      .then(setBoots)
      .catch(() => setBoots([]));
  });

  onCleanup(stopTail);

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
          data-journal-scope={scope()}
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

      {/* What the pane is doing, not buttons to make it do it: history comes in
          by scrolling and the tail runs by itself, so there is nothing here a
          reader would have to press. No line count either — the only number
          this side could offer is how many rows happen to be buffered, which
          answers a question nobody asked.
          And nothing at all while it is tailing: that is what a journal pane is
          for, so saying "Live" told a reader only that the ordinary thing was
          happening. The news is when it stops. */}
      <div
        style={{
          display: "flex",
          gap: `${scale().sm}px`,
          "align-items": "center",
          color: theme().dimFg,
          "font-size": `${scale().sm}px`,
        }}
      >
        {/* The attribute stays either way: it is how a test tells a tailing
            pane from a stopped one now that the pane itself is quiet. */}
        <span data-journal-live={tailing() ? "on" : "off"}>
          <Show when={!tailing()}>
            {canFollow() ? t("systemd.logsPaused") : t("systemd.logsLiveBoot")}
          </Show>
        </span>
        <Show when={busy()}>
          <span>{t("systemd.logsLoading")}</span>
        </Show>
      </div>

      <div
        ref={list}
        data-journal-list
        onScroll={onScroll}
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
              data-journal-row
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
