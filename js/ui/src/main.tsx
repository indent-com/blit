import { render } from "solid-js/web";
import { initWasm } from "./wasm";
import { connectConfigWs } from "./storage";
import { App } from "./App";

const ICON_SVG =
  "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'>" +
  "<rect width='100' height='100' rx='16' fill='%23222'/>" +
  "<text x='12' y='76' font-family='monospace' font-size='72' font-weight='bold' fill='%2358f'>b</text>" +
  "<rect x='60' y='24' width='8' height='52' rx='2' fill='%2358f' opacity='.7'/>" +
  "</svg>";

// Maskable: glyph inset to the center 80% safe zone; OS clips the background.
const MASKABLE_SVG =
  "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'>" +
  "<rect width='100' height='100' fill='%23222'/>" +
  "<g transform='translate(10 10) scale(0.8)'>" +
  "<text x='12' y='76' font-family='monospace' font-size='72' font-weight='bold' fill='%2358f'>b</text>" +
  "<rect x='60' y='24' width='8' height='52' rx='2' fill='%2358f' opacity='.7'/>" +
  "</g></svg>";

// Inject a Web App Manifest dynamically so the app is installable even when
// served as a single inlined HTML file (no separate manifest.json).
{
  const SCREENSHOT_SVG =
    "<svg xmlns='http://www.w3.org/2000/svg' width='1280' height='800'>" +
    "<rect width='1280' height='800' fill='%23111'/>" +
    "<text x='640' y='380' text-anchor='middle' font-family='monospace' font-size='48' font-weight='bold' fill='%2358f'>Blit</text>" +
    "<text x='640' y='440' text-anchor='middle' font-family='monospace' font-size='20' fill='%23888'>terminal multiplexer</text>" +
    "</svg>";

  const manifest = {
    name: "Blit",
    short_name: "Blit",
    description: "Terminal multiplexer for the browser",
    start_url: location.origin + location.pathname,
    display: "standalone",
    background_color: "#000",
    theme_color: "#000",
    icons: [
      {
        src: `data:image/svg+xml,${ICON_SVG}`,
        sizes: "any",
        type: "image/svg+xml",
        purpose: "any",
      },
      {
        src: `data:image/svg+xml,${MASKABLE_SVG}`,
        sizes: "any",
        type: "image/svg+xml",
        purpose: "maskable",
      },
    ],
    screenshots: [
      {
        src: `data:image/svg+xml,${SCREENSHOT_SVG}`,
        sizes: "1280x800",
        type: "image/svg+xml",
        form_factor: "wide",
        label: "Blit terminal multiplexer",
      },
    ],
  };
  const blob = new Blob([JSON.stringify(manifest)], {
    type: "application/json",
  });
  const link = document.createElement("link");
  link.rel = "manifest";
  link.href = URL.createObjectURL(blob);
  document.head.appendChild(link);
}

// Capture the install prompt so the Cmd+K overlay can offer "Install App".
// The browser fires beforeinstallprompt only when the manifest is valid and
// the app isn't already installed.
interface BeforeInstallPromptEvent extends Event {
  prompt(): Promise<void>;
}
let deferredInstallPrompt: BeforeInstallPromptEvent | null = null;
window.addEventListener("beforeinstallprompt", (e) => {
  e.preventDefault();
  deferredInstallPrompt = e as BeforeInstallPromptEvent;
});
window.addEventListener("appinstalled", () => {
  deferredInstallPrompt = null;
});
export function getInstallPrompt(): BeforeInstallPromptEvent | null {
  return deferredInstallPrompt;
}
export function clearInstallPrompt(): void {
  deferredInstallPrompt = null;
}

connectConfigWs();

initWasm().then((wasm) => {
  render(() => <App wasm={wasm} />, document.getElementById("root")!);
});
