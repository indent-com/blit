import { onMount, onCleanup } from "solid-js";

const SHORTCUTS: [string, string][] = [
  ["Mod+Shift+Enter", "New terminal"],
  ["Mod+[ / ]", "Previous / next tab"],
  ["Mod+Shift+D", "Toggle debug panel"],
  ["Ctrl+?", "Toggle this panel"],
];

const MOD_LABEL =
  typeof navigator !== "undefined" && navigator.userAgent.includes("Mac")
    ? "\u2318"
    : "Ctrl";

export default function ShortcutsPanel(props: { onClose: () => void }) {
  onMount(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        props.onClose();
      }
    };
    window.addEventListener("keydown", handler);
    onCleanup(() => window.removeEventListener("keydown", handler));
  });

  return (
    <div
      onClick={props.onClose}
      class="fixed inset-0 z-[9998] flex items-center justify-center bg-black/50"
    >
      <div
        onClick={(e: MouseEvent) => e.stopPropagation()}
        class="bg-[var(--bg)] border border-[var(--border)] rounded-xl p-5 min-w-[300px] font-mono text-[13px] text-[var(--fg)]"
      >
        <div class="font-bold text-sm mb-4">Keyboard shortcuts</div>
        <table style={{ "border-spacing": "0 8px" }}>
          <tbody>
            {SHORTCUTS.map(([key, desc]) => (
              <tr>
                <td class="pr-6 text-[var(--dim)] whitespace-nowrap">
                  {key.replace(/Mod/g, MOD_LABEL)}
                </td>
                <td>{desc}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
