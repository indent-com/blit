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
      class="bg-transparent border border-[var(--border)] text-[var(--dim)] px-2 py-0.5 rounded text-xs font-mono cursor-pointer shrink-0 hover:text-[var(--fg)] hover:border-[var(--dim)] transition-colors"
      aria-label="Copy to clipboard"
    >
      {copied() ? "Copied" : "Copy"}
    </button>
  );
}
