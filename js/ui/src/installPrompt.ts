/**
 * The deferred PWA install prompt, so the Cmd+K overlay can offer
 * "Install App". The browser fires `beforeinstallprompt` only when the
 * manifest is valid and the app isn't already installed.
 *
 * This lives in its own leaf module rather than in `main.tsx` on purpose.
 * `main.tsx` is the Vite HTML entry; anything importing it makes the entry
 * part of an import cycle, and Vite's HMR walk can then find an accepting
 * boundary *above* the entry instead of falling back to a full reload. The
 * entry's module body would re-run, and `render()` appends — so the whole
 * app would mount a second time into `#root`.
 */

interface BeforeInstallPromptEvent extends Event {
  prompt(): Promise<void>;
}

let deferred: BeforeInstallPromptEvent | null = null;

window.addEventListener("beforeinstallprompt", (e) => {
  e.preventDefault();
  deferred = e as BeforeInstallPromptEvent;
});
window.addEventListener("appinstalled", () => {
  deferred = null;
});

export function getInstallPrompt(): BeforeInstallPromptEvent | null {
  return deferred;
}

export function clearInstallPrompt(): void {
  deferred = null;
}
