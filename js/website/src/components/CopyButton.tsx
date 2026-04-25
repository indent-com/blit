import { createSignal } from "solid-js";

export default function CopyButton(props: { text: string }) {
  const [copied, setCopied] = createSignal(false);
  let timeout: ReturnType<typeof setTimeout> | undefined;

  const handleClick = () => {
    navigator.clipboard.writeText(props.text);
    setCopied(true);
    clearTimeout(timeout);
    timeout = setTimeout(() => setCopied(false), 2000);
  };

  return (
    <button
      onClick={handleClick}
      class="inline-flex shrink-0 cursor-pointer items-center gap-1.5 rounded-md border border-[var(--border)] bg-[var(--surface)] px-2 py-1 font-mono text-[11px] text-[var(--dim)] transition-all hover:border-[var(--accent)] hover:bg-[var(--accent-soft)] hover:text-[var(--accent)]"
      aria-label="Copy to clipboard"
    >
      {copied() ? (
        <>
          <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M3 8l3.5 3.5L13 5" />
          </svg>
          <span>Copied</span>
        </>
      ) : (
        <>
          <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
            <rect x="5" y="2.5" width="8.5" height="10" rx="1.5" />
            <path d="M3 5v8.5A1.5 1.5 0 0 0 4.5 15H10" />
          </svg>
          <span>Copy</span>
        </>
      )}
    </button>
  );
}
