import { createSignal } from "solid-js";
import { encryptPassphrase } from "../lib/passphrase-crypto";

export default function JoinForm() {
  const [secret, setSecret] = createSignal("");
  const [visible, setVisible] = createSignal(false);

  const handleSubmit = (e: Event) => {
    e.preventDefault();
    const trimmed = secret().trim();
    if (trimmed) {
      const encrypted = encryptPassphrase(trimmed);
      window.location.href = `/s#${encodeURIComponent(encrypted)}`;
    }
  };

  return (
    <form
      class="inline-flex max-w-full items-stretch overflow-hidden rounded-lg border border-[var(--border-strong)] bg-[var(--bg-elevated)] shadow-sm transition-colors focus-within:border-[var(--accent)]"
      onSubmit={handleSubmit}
    >
      <input
        class="w-72 max-w-full border-none bg-transparent px-3.5 py-2 font-mono text-[13.5px] text-[var(--fg)] outline-none placeholder:text-[var(--dim)] placeholder:opacity-60 max-sm:w-52"
        classList={{ "[-webkit-text-security:disc]": !visible() }}
        type="text"
        placeholder="share secret"
        value={secret()}
        onInput={(e) => setSecret(e.currentTarget.value)}
        spellcheck={false}
        autocomplete="off"
        data-1p-ignore
        data-lpignore="true"
        data-form-type="other"
      />
      <button
        type="button"
        class="flex items-center justify-center border-l border-[var(--border)] bg-transparent px-2.5 text-[var(--dim)] transition-colors hover:text-[var(--fg)]"
        onClick={() => setVisible((v) => !v)}
        aria-label={visible() ? "Hide secret" : "Show secret"}
        tabIndex={-1}
      >
        {visible() ? (
          <svg
            width="16"
            height="16"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <path d="M1 1l14 14M6.5 6.5a2 2 0 0 0 3 3M2.5 5.2C1.6 6.1 1 7 1 8c0 2.2 3.1 5 7 5 .8 0 1.6-.1 2.3-.3M13.5 10.8c.9-.9 1.5-1.8 1.5-2.8 0-2.2-3.1-5-7-5-.8 0-1.6.1-2.3.3" />
          </svg>
        ) : (
          <svg
            width="16"
            height="16"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <path d="M1 8c0 2.2 3.1 5 7 5s7-2.8 7-5-3.1-5-7-5-7 2.8-7 5Z" />
            <circle cx="8" cy="8" r="2" />
          </svg>
        )}
      </button>
      <button
        class="cursor-pointer border-none bg-[var(--accent)] px-4 py-2 font-mono text-[13px] font-semibold text-white transition-opacity hover:opacity-90 disabled:cursor-default disabled:opacity-40"
        type="submit"
        disabled={!secret().trim()}
      >
        Join
      </button>
    </form>
  );
}
