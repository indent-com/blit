import { createSignal, createEffect, onMount, onCleanup, For } from "solid-js";

type DebugEntry = { t: number; level: "log" | "warn" | "error"; msg: string };

export interface DebugLog {
  subscribe(listener: () => void): () => void;
  getSnapshot(): readonly DebugEntry[];
  log(msg: string, ...args: unknown[]): void;
  warn(msg: string, ...args: unknown[]): void;
  error(msg: string, ...args: unknown[]): void;
}

export function createDebugLog(): DebugLog {
  const entries: DebugEntry[] = [];
  let snapshot: readonly DebugEntry[] = [];
  const listeners = new Set<() => void>();

  function push(level: DebugEntry["level"], msg: string, args: unknown[]) {
    const formatted = args.length
      ? msg.replace(/%[sdo]/g, () => {
          const a = args.shift();
          return typeof a === "object" ? JSON.stringify(a) : String(a);
        })
      : msg;
    entries.push({ t: Date.now(), level, msg: formatted });
    if (entries.length > 500) entries.shift();
    snapshot = [...entries];
    for (const l of listeners) l();
  }

  return {
    log(msg: string, ...args: unknown[]) {
      push("log", msg, args);
    },
    warn(msg: string, ...args: unknown[]) {
      push("warn", msg, args);
    },
    error(msg: string, ...args: unknown[]) {
      push("error", msg, args);
    },
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    getSnapshot() {
      return snapshot;
    },
  };
}

export default function DebugPanel(props: {
  log: DebugLog;
  onClose: () => void;
}) {
  const [entries, setEntries] = createSignal<readonly DebugEntry[]>(
    props.log.getSnapshot(),
  );
  const [copied, setCopied] = createSignal(false);
  let bottomRef!: HTMLDivElement;

  onMount(() => {
    const unsub = props.log.subscribe(() =>
      setEntries(props.log.getSnapshot()),
    );
    onCleanup(unsub);
  });

  createEffect(() => {
    entries(); // track
    bottomRef?.scrollIntoView({ behavior: "smooth" });
  });

  const colors = { log: "#8b949e", warn: "#d29922", error: "#f85149" };

  const copyLog = () => {
    const text = entries()
      .map(
        (e) =>
          `${new Date(e.t).toISOString().slice(11, 23)} [${e.level}] ${e.msg}`,
      )
      .join("\n");
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };

  return (
    <div class="fixed top-0 right-0 bottom-0 w-[420px] bg-[rgba(13,17,23,0.95)] border-l border-[#30363d] flex flex-col z-[9999] font-mono text-[11px]">
      <div class="px-3 py-2 border-b border-[#30363d] flex justify-between items-center text-[#c9d1d9] font-bold text-xs">
        <span>blit debug</span>
        <div class="flex gap-1">
          <button
            onClick={copyLog}
            class="bg-transparent border border-[#30363d] text-[#8b949e] cursor-pointer text-[11px] px-2 py-0.5 rounded font-mono"
          >
            {copied() ? "Copied!" : "Copy"}
          </button>
          <button
            onClick={props.onClose}
            class="bg-transparent border-none text-[#8b949e] cursor-pointer text-sm px-1.5"
          >
            {"\u2715"}
          </button>
        </div>
      </div>
      <div class="flex-1 overflow-y-auto py-1">
        <For each={entries()}>
          {(e) => (
            <div
              class="px-3 py-0.5"
              style={{
                color: colors[e.level],
                "border-left":
                  e.level !== "log"
                    ? `2px solid ${colors[e.level]}`
                    : "2px solid transparent",
              }}
            >
              <span class="opacity-50">
                {new Date(e.t).toISOString().slice(11, 23)}
              </span>{" "}
              {e.msg}
            </div>
          )}
        </For>
        <div ref={bottomRef} />
      </div>
    </div>
  );
}
