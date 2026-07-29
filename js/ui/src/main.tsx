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
  // Idempotent for the same reason the mount below is: appending a second
  // manifest link would leave the document with two.
  const link =
    document.head.querySelector<HTMLLinkElement>('link[rel="manifest"]') ??
    document.head.appendChild(document.createElement("link"));
  link.rel = "manifest";
  link.href = URL.createObjectURL(blob);
}

connectConfigWs();

initWasm().then((wasm) => {
  // Mount idempotently. `render()` appends and never clears, so a second
  // execution of this module body would leave two whole app trees in
  // `#root` — two docks, two BSP containers fighting over the same
  // workspace's visible sessions, and a document twice the viewport tall.
  // Nothing should re-execute the entry (see installPrompt.ts on why the
  // entry must stay importer-free), but the guard is cheap and the failure
  // mode is not.
  (import.meta.hot?.data?.dispose as (() => void) | undefined)?.();
  // Not `getElementById("root")!` — that assertion turned a missing mount
  // point into "Uncaught (in promise) Error: The `element` passed to
  // render(...) doesn't exist", which names the symptom and not the cause.
  // The usual cause is a document that is not index.html (a stray dev
  // entry, a stale tab), so say that.
  const root = document.getElementById("root");
  if (!root) {
    throw new Error(
      "blit: no #root element in this document — index.html is the only " +
        "page that hosts the app; a stale or hand-written entry will not work",
    );
  }
  const dispose = render(() => <App wasm={wasm} />, root);
  if (import.meta.hot) import.meta.hot.data.dispose = dispose;
});
