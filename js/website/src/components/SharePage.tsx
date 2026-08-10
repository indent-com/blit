/**
 * blit.sh/s — open a share link in the full blit workspace.
 *
 * This used to mount a 900-line reimplementation of the app (tabs, status
 * overlay, mobile toolbar, debug panel) that had drifted from the product it
 * imitated. It now mounts the product: `@blit-sh/ui`'s workspace over a
 * WebRTC share transport, so a share link gets BSP panes, the IDE dock, and
 * every future feature the app grows, for free.
 *
 * What this file still owns is the link itself: the `#psk=` fragment, its
 * at-rest encryption, and the read-only token rule — semantics of the URL,
 * not of the workspace.
 */

import { createSignal, onMount, Show } from "solid-js";
import type { BlitWasmModule } from "@blit-sh/core";
import { mountBlitWorkspace, shareTransport } from "@blit-sh/ui/embed";
import { initWasm } from "../lib/wasm";
import { MONO_CATALOG, MONO_STACK } from "../lib/fonts";
import {
  isEncrypted,
  encryptPassphrase,
  decryptPassphrase,
} from "../lib/passphrase-crypto";

const HUB_URL = "wss://hub.blit.sh";

type PassphraseResult =
  | { ok: true; passphrase: string; readOnly: boolean; debug: boolean }
  | { ok: false; error: string };

/** Where the last-opened share's secret lives, so a refresh — or a link
 *  whose encrypted blob was minted on another device — can still connect
 *  from a browser that has opened this share before. */
const LAST_SHARE_KEY = "blit-share-last-psk";

const ok = (passphrase: string, debug = false): PassphraseResult => ({
  ok: true,
  passphrase,
  readOnly: passphrase.endsWith(".ro"),
  debug,
});

/**
 * The share passphrase from the URL fragment.
 *
 * `blit share` prints links in the canonical first-contact form the app
 * itself parses: `#psk=<url-encoded passphrase>`, possibly among other
 * `&`-separated fragment parts. Treating the whole fragment as the
 * passphrase — as this page once did — connects with the literal string
 * `psk=…` and fails against the hub.
 *
 * A plaintext passphrase is remembered for this browser and re-written
 * over the URL encrypted with a device-local key, so a synced history or
 * a screenshot of the address bar does not leak terminal access. When an
 * encrypted blob cannot be decrypted (it was minted on another device),
 * the remembered secret is the fallback before giving up.
 */
/** Fragment parts this page owns rather than passes through. */
const FLAG_PARTS = new Set(["debug"]);

function resolvePassphrase(): PassphraseResult {
  // The fragment is split once, and flags are removed before anything reads
  // a secret out of it. Doing this per-branch is how `&debug` ended up
  // *inside* a legacy passphrase — which the hub then refused, and which
  // was persisted and re-encrypted into the URL, so it outlived the flag.
  const parts = location.hash.slice(1).split("&").filter(Boolean);
  const debug = parts.some((p) => FLAG_PARTS.has(p));
  const secretParts = parts.filter((p) => !FLAG_PARTS.has(p));
  const stored = (() => {
    try {
      return localStorage.getItem(LAST_SHARE_KEY);
    } catch {
      return null;
    }
  })();
  if (secretParts.length === 0) {
    return stored
      ? ok(stored, debug)
      : { ok: false, error: "No share link specified." };
  }

  // Canonical form: a `psk=` part, as `blit share` prints it.
  const psk = secretParts
    .map((part) => {
      const eq = part.indexOf("=");
      return eq > 0 && decodeURIComponent(part.slice(0, eq)) === "psk"
        ? decodeURIComponent(part.slice(eq + 1))
        : null;
    })
    .find((v) => v !== null);
  // Legacy/bare form: the first non-flag part is the whole secret.
  const bare = decodeURIComponent(secretParts[0]);
  const plaintext = psk ?? (isEncrypted(bare) ? null : bare);

  if (plaintext !== null) {
    try {
      localStorage.setItem(LAST_SHARE_KEY, plaintext);
    } catch {
      // Private windows still get this one session.
    }
    // Read-only tokens stay legible on purpose: they carry no write
    // authority and exist to be passed around as URLs.
    if (!plaintext.endsWith(".ro")) {
      try {
        const encrypted = encryptPassphrase(plaintext);
        const nextHash = [
          encodeURIComponent(encrypted),
          ...(debug ? ["debug"] : []),
        ].join("&");
        history.replaceState(null, "", `/s#${nextHash}`);
      } catch (e) {
        console.error("[blit] encryptPassphrase failed:", e);
        // Fall through — still return the plaintext passphrase.
      }
    }
    return ok(plaintext, debug);
  }

  const decrypted = decryptPassphrase(bare);
  if (decrypted !== null) return ok(decrypted, debug);
  if (stored) return ok(stored, debug);
  return {
    ok: false,
    error: "Cannot decrypt link. This link was created on a different device.",
  };
}

function Notice(props: { title: string; body: string }) {
  return (
    <div class="flex h-full items-center justify-center px-6">
      <div class="max-w-md text-center">
        <div class="font-mono text-[13px] font-bold uppercase tracking-widest text-[var(--dim)]">
          {props.title}
        </div>
        <p class="mt-3 text-[15px] leading-relaxed text-[var(--fg)]">
          {props.body}
        </p>
        <a
          href="/"
          class="mt-6 inline-block text-[13px] text-[var(--accent)] no-underline hover:underline"
        >
          ← blit.sh
        </a>
      </div>
    </div>
  );
}

export default function SharePage() {
  const [result, setResult] = createSignal<PassphraseResult | null>(null);
  const [wasm, setWasm] = createSignal<BlitWasmModule | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  onMount(() => {
    const r = resolvePassphrase();
    setResult(r);
    if (r.ok) {
      initWasm()
        .then(setWasm)
        .catch((e) => setError(String(e)));
    }
  });

  const failure = () => {
    const r = result();
    if (r && !r.ok) return r.error;
    return error();
  };

  return (
    <Show
      when={!failure()}
      fallback={<Notice title="Cannot connect" body={failure()!} />}
    >
      <Show
        when={result()?.ok && wasm()}
        fallback={
          <div class="flex h-full items-center justify-center font-mono text-[13px] text-[var(--dim)]">
            loading…
          </div>
        }
      >
        {(_) => {
          const r = result() as {
            ok: true;
            passphrase: string;
            readOnly: boolean;
            debug: boolean;
          };
          return (
            <div
              class="h-full w-full"
              ref={(el) => {
                mountBlitWorkspace(el, {
                  wasm: wasm()!,
                  // The site's face, already self-hosted by the layout, so
                  // the shared terminal matches the page it opened from
                  // instead of falling back to the platform's monospace.
                  fontFamily: MONO_STACK,
                  // The picker's whole menu: this origin serves no
                  // `font/<family>` route, so a typed name would load
                  // nothing and read as a broken control.
                  fonts: MONO_CATALOG,
                  connections: [
                    {
                      id: "share",
                      label: "shared terminal",
                      transport: shareTransport(
                        HUB_URL,
                        r.passphrase,
                        // `#…&debug` on the fragment narrates signaling to
                        // the console — connection problems on a static
                        // page are otherwise invisible. The workspace also
                        // reads and maintains the same flag for its pane.
                        r.debug ? console : undefined,
                      ),
                      // A `.ro` watch link: the server refuses writes, so
                      // the workspace renders its terminals read-only
                      // rather than swallowing keystrokes silently.
                      readOnly: r.readOnly,
                    },
                  ],
                  onAuthError: () =>
                    setError(
                      "The share was refused — the link may have been revoked.",
                    ),
                });
              }}
            />
          );
        }}
      </Show>
    </Show>
  );
}
