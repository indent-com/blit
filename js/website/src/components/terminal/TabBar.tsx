import {
  createSignal,
  createEffect,
  onCleanup,
  For,
  type Accessor,
} from "solid-js";
import type { BlitSession, SessionId } from "@blit-sh/core";

const TAB_ANIMATION_MS = 200;
const TITLE_DEBOUNCE_MS = 150;

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
  sessionId: SessionId;
  liveIndex: number;
  exiting: boolean;
};

function createAnimatedTabs(
  sessionsAccessor: Accessor<readonly BlitSession[]>,
) {
  const [displayTabs, setDisplayTabs] = createSignal<DisplayTab[]>(
    sessionsAccessor().map((s, i) => ({
      sessionId: s.id,
      liveIndex: i,
      exiting: false,
    })),
  );
  const [enteringIds, setEnteringIds] = createSignal<Set<string>>(new Set());
  let prevIds = new Set(sessionsAccessor().map((s) => s.id));
  const exitTimers = new Map<string, ReturnType<typeof setTimeout>>();

  createEffect(() => {
    const sessions = sessionsAccessor();
    const currentIds = new Set(sessions.map((s) => s.id));

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
      // Only session data changed (title, etc) — no structural changes,
      // so don't touch displayTabs at all. Tab components read session
      // data reactively from the sessions prop.
      return;
    }

    // Build new display list — reuse existing DisplayTab objects so <For>
    // doesn't remount Tab components.
    setDisplayTabs((prev) => {
      const result: DisplayTab[] = prev.map((dt) => {
        if (removed.has(dt.sessionId)) {
          dt.exiting = true;
        }
        return dt;
      });

      for (const s of sessions) {
        if (added.has(s.id)) {
          result.push({ sessionId: s.id, liveIndex: -1, exiting: false });
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
        setDisplayTabs((prev) => prev.filter((dt) => dt.sessionId !== id));
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
        if (enteringIds().has(dt.sessionId)) return "0fr";
        return "1fr";
      })
      .join(" ");

  return { displayTabs, gridTemplateColumns };
}

// ---------------------------------------------------------------------------
// Tab
// ---------------------------------------------------------------------------

function Tab(props: {
  sessionId: SessionId;
  getTitle: () => string | null | undefined;
  index: number;
  isFocused: boolean;
  exiting: boolean;
  onSelect: (id: SessionId) => void;
  onClose: (id: SessionId) => void;
}) {
  const label = createStableTitle(
    props.getTitle,
    () => `Tab ${props.index + 1}`,
  );

  return (
    <div class="min-w-0 overflow-hidden">
      <div
        role="button"
        tabIndex={0}
        onClick={() => !props.exiting && props.onSelect(props.sessionId)}
        onAuxClick={(e: MouseEvent) => {
          if (e.button === 1 && !props.exiting) {
            e.preventDefault();
            props.onClose(props.sessionId);
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
            if (!props.exiting) props.onClose(props.sessionId);
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
  sessions: readonly BlitSession[];
  focusedSessionId: SessionId | null;
  onSelect: (id: SessionId) => void;
  onClose: (id: SessionId) => void;
  disabled?: boolean;
}) {
  const { displayTabs, gridTemplateColumns } = createAnimatedTabs(
    () => props.sessions,
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
              sessionId={dt.sessionId}
              getTitle={() =>
                props.sessions.find((s) => s.id === dt.sessionId)?.title
              }
              index={dt.liveIndex}
              isFocused={!dt.exiting && dt.sessionId === props.focusedSessionId}
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
