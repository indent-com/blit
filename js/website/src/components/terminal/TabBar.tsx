import {
  createSignal,
  createEffect,
  onCleanup,
  For,
  type Accessor,
} from "solid-js";

const TAB_ANIMATION_MS = 200;
const TITLE_DEBOUNCE_MS = 150;

// ---------------------------------------------------------------------------
// Tab item
// ---------------------------------------------------------------------------

/** A generic tab — either a terminal session or a Wayland surface. */
export type TabItem = {
  /** Stable identifier — e.g. `session:<id>` or `surface:<connId>:<sid>`. */
  id: string;
  /** Title displayed in the tab; null falls back to "Tab N". */
  title: string | null;
  /** Optional fallback label used while `title` is null. Defaults to `Tab N`. */
  fallback?: string;
};

// ---------------------------------------------------------------------------
// Debounced, stable title
// ---------------------------------------------------------------------------

function createStableTitle(
  titleAccessor: Accessor<string | null | undefined>,
  fallback: Accessor<string>,
): Accessor<string> {
  const initial = titleAccessor();
  const [stable, setStable] = createSignal<string | null>(initial || null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let first = true;

  createEffect(() => {
    const title = titleAccessor();
    if (!title) return;
    // Show the first title immediately — only debounce subsequent changes.
    if (first) {
      first = false;
      setStable(title);
      return;
    }
    clearTimeout(timer);
    timer = setTimeout(() => {
      setStable(title);
      timer = undefined;
    }, TITLE_DEBOUNCE_MS);
  });

  onCleanup(() => clearTimeout(timer));

  return () => stable() || fallback();
}

// ---------------------------------------------------------------------------
// Animated tab list
// ---------------------------------------------------------------------------

type DisplayTab = {
  tabId: string;
  liveIndex: number;
  exiting: boolean;
};

function createAnimatedTabs(tabsAccessor: Accessor<readonly TabItem[]>) {
  const [displayTabs, setDisplayTabs] = createSignal<DisplayTab[]>(
    tabsAccessor().map((t, i) => ({
      tabId: t.id,
      liveIndex: i,
      exiting: false,
    })),
  );
  const [enteringIds, setEnteringIds] = createSignal<Set<string>>(new Set());
  let prevIds = new Set(tabsAccessor().map((t) => t.id));
  const exitTimers = new Map<string, ReturnType<typeof setTimeout>>();

  createEffect(() => {
    const tabs = tabsAccessor();
    const currentIds = new Set(tabs.map((t) => t.id));

    const added = new Set<string>();
    for (const id of currentIds) {
      if (!prevIds.has(id)) added.add(id);
    }

    const removed = new Set<string>();
    for (const id of prevIds) {
      if (!currentIds.has(id)) removed.add(id);
    }

    prevIds = currentIds;

    if (added.size === 0 && removed.size === 0) {
      // Only tab data changed (title, etc) — no structural changes,
      // so don't touch displayTabs at all. Tab components read data
      // reactively from the tabs prop.
      return;
    }

    // Build new display list — reuse existing DisplayTab objects so <For>
    // doesn't remount Tab components.
    setDisplayTabs((prev) => {
      const result: DisplayTab[] = prev.map((dt) => {
        if (removed.has(dt.tabId)) {
          dt.exiting = true;
        }
        return dt;
      });

      for (const t of tabs) {
        if (added.has(t.id)) {
          result.push({ tabId: t.id, liveIndex: -1, exiting: false });
        }
      }

      let liveIdx = 0;
      for (const dt of result) {
        if (!dt.exiting) dt.liveIndex = liveIdx++;
      }
      return result;
    });

    if (added.size > 0) {
      setEnteringIds(added);
    }

    for (const id of removed) {
      const existing = exitTimers.get(id);
      if (existing) clearTimeout(existing);

      const timer = setTimeout(() => {
        setDisplayTabs((prev) => prev.filter((dt) => dt.tabId !== id));
        exitTimers.delete(id);
      }, TAB_ANIMATION_MS + 50);
      exitTimers.set(id, timer);
    }
  });

  // Clear entering IDs on next frame
  createEffect(() => {
    const entering = enteringIds();
    if (entering.size === 0) return;
    const raf = requestAnimationFrame(() => setEnteringIds(new Set()));
    onCleanup(() => cancelAnimationFrame(raf));
  });

  onCleanup(() => {
    for (const timer of exitTimers.values()) clearTimeout(timer);
  });

  const gridTemplateColumns = () =>
    displayTabs()
      .map((dt) => {
        if (dt.exiting) return "0fr";
        if (enteringIds().has(dt.tabId)) return "0fr";
        return "1fr";
      })
      .join(" ");

  return { displayTabs, gridTemplateColumns };
}

// ---------------------------------------------------------------------------
// Tab
// ---------------------------------------------------------------------------

function Tab(props: {
  tabId: string;
  getTitle: () => string | null | undefined;
  getFallback: () => string;
  isFocused: boolean;
  exiting: boolean;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
}) {
  const label = createStableTitle(props.getTitle, () => props.getFallback());

  return (
    <div class="min-w-0 overflow-hidden">
      <div
        role="button"
        tabIndex={0}
        onClick={() => !props.exiting && props.onSelect(props.tabId)}
        onAuxClick={(e: MouseEvent) => {
          if (e.button === 1 && !props.exiting) {
            e.preventDefault();
            props.onClose(props.tabId);
          }
        }}
        class={`group relative flex h-full w-full min-w-0 cursor-pointer items-center whitespace-nowrap border-r border-r-[var(--border)] font-sans text-xs transition-colors ${
          props.isFocused
            ? "bg-[var(--bg)] font-medium text-[var(--fg)]"
            : "bg-transparent font-normal text-[var(--dim)] hover:bg-[var(--bg)]"
        }`}
      >
        {/* Close button — left-aligned, visible on hover */}
        <button
          type="button"
          tabIndex={-1}
          onClick={(e: MouseEvent) => {
            e.stopPropagation();
            if (!props.exiting) props.onClose(props.tabId);
          }}
          class="absolute left-1.5 flex h-[18px] w-[18px] shrink-0 cursor-pointer items-center justify-center rounded border-none bg-transparent p-0 text-[var(--dim)] text-xs leading-none opacity-0 transition-[opacity,background-color,color] duration-100 hover:bg-[var(--surface)] hover:text-[var(--fg)] group-hover:opacity-100"
        >
          {"\u00D7"}
        </button>
        {/* Title — centered */}
        <span class="w-full overflow-hidden text-ellipsis px-6 text-center">
          {label()}
        </span>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// TabBar
// ---------------------------------------------------------------------------

export default function TabBar(props: {
  tabs: readonly TabItem[];
  focusedId: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  disabled?: boolean;
}) {
  const { displayTabs, gridTemplateColumns } = createAnimatedTabs(
    () => props.tabs,
  );

  return (
    <div
      class={`flex h-9 min-h-9 select-none items-stretch overflow-hidden bg-[var(--surface)] transition-opacity ${
        props.disabled ? "opacity-50 pointer-events-none" : ""
      }`}
    >
      <div
        class="grid min-w-0 flex-1 items-stretch transition-[grid-template-columns] duration-200 ease-out"
        style={{ "grid-template-columns": gridTemplateColumns() }}
      >
        <For each={displayTabs()}>
          {(dt) => (
            <Tab
              tabId={dt.tabId}
              getTitle={() => {
                const t = props.tabs.find((t) => t.id === dt.tabId);
                return t?.title ?? null;
              }}
              getFallback={() => {
                const t = props.tabs.find((t) => t.id === dt.tabId);
                return t?.fallback ?? `Tab ${dt.liveIndex + 1}`;
              }}
              isFocused={!dt.exiting && dt.tabId === props.focusedId}
              exiting={dt.exiting}
              onSelect={props.onSelect}
              onClose={props.onClose}
            />
          )}
        </For>
      </div>
    </div>
  );
}
