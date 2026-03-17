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
      class="inline-flex items-center border border-[var(--border)] rounded overflow-hidden bg-[var(--surface)] focus-within:border-[var(--accent)]"
      onSubmit={handleSubmit}
    >
      <input
        class="bg-transparent border-none outline-none text-[var(--fg)] font-mono text-sm px-3 py-1.5 w-64 max-sm:w-48 placeholder:text-[var(--dim)] placeholder:opacity-60"
        classList={{ "[-webkit-text-security:disc]": !visible() }}
        type="text"
        placeholder="session secret"
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
        class="flex items-center justify-center bg-transparent border-none border-l border-l-[var(--border)] text-[var(--dim)] px-2 cursor-pointer hover:text-[var(--fg)] transition-colors"
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
        class="px-3 py-1.5 bg-[var(--accent)] text-white border-none font-mono text-[13px] font-semibold cursor-pointer hover:opacity-85 disabled:opacity-40 disabled:cursor-default transition-opacity"
        type="submit"
        disabled={!secret().trim()}
      >
        Join
      </button>
    </form>
  );
}
