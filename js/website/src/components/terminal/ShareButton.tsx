import { createSignal } from "solid-js";

export default function ShareButton(props: { passphrase: string }) {
  const [copied, setCopied] = createSignal(false);
  let timeout: ReturnType<typeof setTimeout> | undefined;

  const handleClick = (e: MouseEvent) => {
    e.stopPropagation();
    const url = `${location.origin}/s#${encodeURIComponent(props.passphrase)}`;
    navigator.clipboard.writeText(url);
    setCopied(true);
    clearTimeout(timeout);
    timeout = setTimeout(() => setCopied(false), 2000);
  };

  return (
    <button
      onClick={handleClick}
      class="flex items-center gap-1.5 px-2.5 bg-transparent border-none text-[var(--dim)] cursor-pointer text-xs font-mono whitespace-nowrap shrink-0 transition-colors hover:text-[var(--fg)]"
      title="Copy share link"
    >
      <svg
        width="14"
        height="14"
        viewBox="0 0 16 16"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
        stroke-linecap="round"
        stroke-linejoin="round"
      >
        <path d="M4 12a2 2 0 1 1 0-4 2 2 0 0 1 0 4ZM12 6a2 2 0 1 1 0-4 2 2 0 0 1 0 4ZM12 16a2 2 0 1 1 0-4 2 2 0 0 1 0 4ZM5.7 9.3l4.6 3.4M10.3 5.3l-4.6 3.4" />
      </svg>
      {copied() ? "Copied!" : "Share"}
    </button>
  );
}
