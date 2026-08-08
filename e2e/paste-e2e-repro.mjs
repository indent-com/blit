/* Live end-to-end paste-chain repro against a real blit server+gateway.
 *
 * Stages probed:
 *  (A) browser -> server websocket: C2S_CLIPBOARD_SET (0x25) and the
 *      C2S_SURFACE_INPUT (0x20) V keypress are sniffed off the wire.
 *  (B/C) server -> compositor -> wayland client: the paste_probe client
 *      (crates/compositor/examples/paste_probe.rs) logs every offer,
 *      selection, key, and the bytes receive() returns.
 *  (D) is the probe reading the bytes itself.
 *
 * Run: node e2e/paste-e2e-repro.mjs   (server+gateway must be up on 3391)
 */
import { chromium } from "@playwright/test";
import { spawn } from "child_process";

const BASE = "http://127.0.0.1:3391";
const WAYLAND_SOCK = "/tmp/blit-paste-repro/wayland-0";

const clientLog = [];
const wsLog = [];

// --- start the wayland probe client ---
const probe = spawn(
  "/src/blit/target/debug/examples/paste_probe",
  [WAYLAND_SOCK],
  { stdio: ["pipe", "pipe", "inherit"] },
);
let probeStdout = "";
probe.stdout.on("data", (d) => {
  probeStdout += d;
  for (const line of String(d).split("\n")) {
    if (line.trim()) clientLog.push(`[client] ${line}`);
  }
});
probe.on("exit", (code) => clientLog.push(`[client] EXIT code=${code}`));

function waitFor(fn, timeoutMs, what) {
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const iv = setInterval(() => {
      const v = fn();
      if (v) {
        clearInterval(iv);
        resolve(v);
      } else if (Date.now() - t0 > timeoutMs) {
        clearInterval(iv);
        reject(new Error(`timeout waiting for ${what}`));
      }
    }, 50);
  });
}

await waitFor(() => probeStdout.includes("READY"), 10000, "probe READY");

// --- browser ---
const browser = await chromium.launch({
  executablePath: "/etc/profiles/per-user/pcarrier/bin/chromium",
});
const context = await browser.newContext();
await context.grantPermissions(["clipboard-read", "clipboard-write"], {
  origin: BASE,
});
const page = await context.newPage();

page.on("websocket", (ws) => {
  wsLog.push(`[ws] open ${ws.url()}`);
  ws.on("framesent", (frame) => {
    const p = frame.payload;
    if (typeof p === "string") {
      wsLog.push(`[ws] sent text ${JSON.stringify(p.slice(0, 80))}`);
      return;
    }
    const b = Buffer.from(p);
    const op = b[0];
    if (op === 0x25 || op === 0x33) {
      const mimeLen = b.readUInt16LE(1);
      const mime = b.subarray(3, 3 + mimeLen).toString();
      const dataLen = b.readUInt32LE(3 + mimeLen);
      wsLog.push(
        `[ws] sent 0x${op.toString(16)} CLIPBOARD_SET mime=${mime} dataLen=${dataLen}`,
      );
    } else if (op === 0x20) {
      const surfaceId = b.readUInt16LE(1);
      const keys = [];
      for (let off = 3; off + 5 <= b.length; off += 5) {
        keys.push(`evdev=${b.readUInt32LE(off)}:${b[off + 4] ? "down" : "up"}`);
      }
      wsLog.push(
        `[ws] sent 0x20 SURFACE_INPUT surface=${surfaceId} keys=${keys.join(",")}`,
      );
    } else {
      wsLog.push(`[ws] sent op=0x${op.toString(16)} len=${b.length}`);
    }
  });
  ws.on("framereceived", (frame) => {
    const p = frame.payload;
    if (typeof p === "string") return;
    const b = Buffer.from(p);
    if (b[0] === 0x25) {
      const mimeLen = b.readUInt16LE(1);
      const mime = b.subarray(3, 3 + mimeLen).toString();
      const dataLen = b.readUInt32LE(3 + mimeLen);
      const text = b.subarray(7 + mimeLen, 7 + mimeLen + Math.min(dataLen, 64)).toString();
      wsLog.push(
        `[ws] recv 0x25 CLIPBOARD_CONTENT mime=${mime} dataLen=${dataLen} [${text}]`,
      );
    }
  });
});

function dump() {
  console.log("================ WS / UI LOG ================");
  console.log(wsLog.join("\n"));
  console.log("================ FULL CLIENT LOG ================");
  console.log(clientLog.join("\n"));
}

// In-page sniffer: wrap WebSocket.send before the app loads (wire frames are
// compressed by permessage-deflate, so page.on('websocket') sees garbage).
// Also record keydown/paste events at document level.
await context.addInitScript(() => {
  window.__wsSent = [];
  window.__evts = [];
  const origSend = WebSocket.prototype.send;
  WebSocket.prototype.send = function (data) {
    try {
      const note = (b) => {
        const u8 = new Uint8Array(b);
        const op = u8[0];
        const dv = new DataView(b);
        if (op === 0x25 || op === 0x33) {
          const mimeLen = dv.getUint16(1, true);
          const mime = new TextDecoder().decode(u8.subarray(3, 3 + mimeLen));
          const dataLen = dv.getUint32(3 + mimeLen, true);
          window.__wsSent.push(
            `0x${op.toString(16)} CLIPBOARD_SET mime=${mime} dataLen=${dataLen}`,
          );
        } else if (op === 0x20) {
          const surfaceId = dv.getUint16(1, true);
          const keys = [];
          for (let off = 3; off + 5 <= u8.length; off += 5)
            keys.push(`evdev=${dv.getUint32(off, true)}:${u8[off + 4] ? "down" : "up"}`);
          window.__wsSent.push(`0x20 SURFACE_INPUT surface=${surfaceId} ${keys.join(",")}`);
        } else if (op === 0x24) {
          window.__wsSent.push(`0x24 SURFACE_FOCUS surface=${dv.getUint16(1, true)}`);
        }
      };
      if (data instanceof ArrayBuffer) note(data);
      else if (ArrayBuffer.isView(data)) note(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
      else if (data instanceof Blob) data.arrayBuffer().then(note);
    } catch {}
    return origSend.call(this, data);
  };
  document.addEventListener(
    "keydown",
    (e) =>
      window.__evts.push(
        `keydown key=${e.key} code=${e.code} ctrl=${e.ctrlKey} target=${e.target?.tagName}/${e.target?.getAttribute?.("aria-label") ?? ""}`,
      ),
    true,
  );
  document.addEventListener(
    "paste",
    (e) => {
      const items = [];
      if (e.clipboardData)
        for (const it of e.clipboardData.items) items.push(it.kind + ":" + it.type);
      window.__evts.push(
        `paste target=${e.target?.tagName}/${e.target?.getAttribute?.("aria-label") ?? ""} items=[${items.join(",")}] text=${JSON.stringify(e.clipboardData?.getData("text/plain") ?? "")}`,
      );
    },
    true,
  );
});

const drainPage = async (label) => {
  const s = await page.evaluate(() => {
    const s = window.__wsSent.splice(0);
    const e = window.__evts.splice(0);
    return { s, e };
  });
  for (const l of s.s) wsLog.push(`[page-ws] ${label} sent ${l}`);
  for (const l of s.e) wsLog.push(`[page-evt] ${label} ${l}`);
};

try {
await page.goto(`${BASE}/#psk=test-secret`);
// Wait for the workspace: the probe's surface should show up as a pane with
// a "Surface input" textarea.
await waitFor(
  () => wsLog.length > 0,
  10000,
  "websocket open",
);
const surfaceInput = page.locator('textarea[aria-label="Surface input"]');
await surfaceInput.waitFor({ state: "attached", timeout: 15000 });
wsLog.push("[ui] surface pane present");
await page.waitForTimeout(2000); // let frames flow so _displaySize is set

// Click the canvas in the same pane as the surface textarea.
const clickPoint = await surfaceInput.evaluate((el) => {
  let node = el.parentElement;
  let canvas = null;
  while (node && !canvas) {
    canvas = node.querySelector("canvas");
    node = node.parentElement;
  }
  if (!canvas) return null;
  const r = canvas.getBoundingClientRect();
  return { x: r.x + r.width / 2, y: r.y + r.height / 2, w: r.width, h: r.height };
});
wsLog.push(`[ui] canvas rect: ${JSON.stringify(clickPoint)}`);
if (clickPoint) await page.mouse.click(clickPoint.x, clickPoint.y);
wsLog.push("[ui] clicked surface canvas");

await waitFor(
  () => probeStdout.includes("KBD-ENTER"),
  8000,
  "probe KBD-ENTER (surface focused)",
).catch(async (e) => {
  wsLog.push(`[warn] ${e.message}`);
  wsLog.push(
    `[warn] activeElement=${await page.evaluate(() => document.activeElement?.outerHTML?.slice(0, 200))}`,
  );
});
await drainPage("after-focus");

// --- Test 1: image paste ---
wsLog.push("=== TEST 1: image/png paste ===");
await page.evaluate(async () => {
  const c = document.createElement("canvas");
  c.width = 8;
  c.height = 8;
  const g = c.getContext("2d");
  g.fillStyle = "#f00";
  g.fillRect(0, 0, 8, 8);
  const blob = await new Promise((r) => c.toBlob(r, "image/png"));
  await navigator.clipboard.write([new ClipboardItem({ "image/png": blob })]);
});
wsLog.push("[ui] clipboard now holds image/png");
const t1Mark = clientLog.length;
await page.keyboard.press("Control+V");
await page.waitForTimeout(2500);
await drainPage("test1");
wsLog.push(`[test1] client log after Ctrl+V:\n${clientLog.slice(t1Mark).join("\n")}`);

// --- Test 2: text paste freshness ---
wsLog.push("=== TEST 2: text paste freshness ===");
await page.evaluate(() => navigator.clipboard.writeText("MARKER-FROM-BROWSER"));
const t2Mark = clientLog.length;
await page.keyboard.press("Control+V");
await page.waitForTimeout(2500);
await drainPage("test2");
wsLog.push(`[test2] client log after Ctrl+V:\n${clientLog.slice(t2Mark).join("\n")}`);

// --- Test 3: copy-out (wayland client -> browser clipboard) ---
wsLog.push("=== TEST 3: copy-out ===");
probe.stdin.write("copy\n");
await page.waitForTimeout(2500);
let browserClip = "<readText failed>";
try {
  browserClip = await page.evaluate(() => navigator.clipboard.readText());
} catch (e) {
  browserClip = `<readText threw: ${e}>`;
}
wsLog.push(`[test3] browser clipboard readText() = ${JSON.stringify(browserClip)}`);

// --- summary ---
} catch (e) {
  wsLog.push(`[fatal] ${e}`);
  try {
    await page.screenshot({ path: "/tmp/blit-paste-repro/failure.png" });
    const html = await page.evaluate(() =>
      document.body.innerHTML.slice(0, 4000),
    );
    wsLog.push(`[dom] ${html}`);
  } catch {}
}
dump();

await browser.close();
probe.kill();
