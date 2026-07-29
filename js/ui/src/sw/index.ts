/** Preview service worker (docs/design/net.md § Client: service worker). */

import {
  bodyStream,
  encodeRequestHead,
  parseBootstrapUrl,
  parsePreviewFrameUrl,
  parseResponseHead,
  previewKey,
  PREVIEW_PREFIX,
  type ParsedBootstrap,
  type PreviewTarget,
  type ResponseHead,
} from "@blit-sh/core";
import { CookieJar } from "./cookies";
import { RelayPool } from "./conn";
import { forgetBinding, loadBindings, rememberBinding } from "./bindings";
import { bootstrapDocument } from "./bootstrap";
import { injectIntoHtml } from "./inject";

// The bundle runs in a worker; the app's tsconfig covers both lib sets, so name the scope explicitly rather than relying on ambient inference.
declare const self: ServiceWorkerGlobalScope & typeof globalThis;

const pool = new RelayPool();
/** clientId → target. Persisted, because after the bootstrap redirect the
 *  frame's own URL is `/` and no longer says what it is bound to. */
const bindings = new Map<string, PreviewTarget>();
/** Warm the map from storage, bounded: no request may wait on IndexedDB.
 *  A miss costs one bootstrap round trip, a hang would cost the navigation. */
const restored = Promise.race([
  loadBindings().then((saved) => {
    for (const [id, target] of saved) {
      if (!bindings.has(id)) bindings.set(id, target);
    }
  }),
  new Promise<void>((resolve) => setTimeout(resolve, 500)),
]);
/** One jar per relayed origin. */
const jars = new Map<string, CookieJar>();

self.addEventListener("install", () => {
  // Take over without a reload: a preview pane that appears before the worker is active would otherwise fetch straight past it and render the blit UI.
  void self.skipWaiting();
});

self.addEventListener("activate", (event: ExtendableEvent) => {
  event.waitUntil(Promise.all([self.clients.claim(), restored, sweep()]));
});

/** Drop bindings for clients that no longer exist. */
async function sweep(): Promise<void> {
  const live = new Set((await self.clients.matchAll()).map((c) => c.id));
  for (const id of [...bindings.keys()]) {
    if (!live.has(id)) {
      bindings.delete(id);
      void forgetBinding(id);
    }
  }
}

/**
 * The preview a client *is*, or null when it is the app itself.
 *
 * Every message below is same-origin, and a previewed page runs on this
 * origin too — so "same-origin" says nothing about who sent it. The binding
 * map is authoritative because only the fetch handler writes it, from the
 * navigation's own URL; parsing the client's URL covers the window between
 * the navigation and the binding. A previewed SPA rewrites its own URL with
 * `pushState`, which is why the binding is consulted first.
 */
function senderPreview(source: Client | null): PreviewTarget | null {
  if (!source) return null;
  const bound = bindings.get(source.id);
  if (bound) return bound;
  try {
    const url = new URL(source.url);
    return (
      parsePreviewFrameUrl(url.pathname, url.search)?.target ??
      parseBootstrapUrl(url.pathname, url.search)?.target ??
      null
    );
  } catch {
    return null;
  }
}

self.addEventListener("message", (event: ExtendableMessageEvent) => {
  const data = event.data as {
    type?: string;
    passphrase?: string;
    target?: PreviewTarget;
    value?: string;
  } | null;
  if (!data || typeof data.type !== "string") return;
  if (data.type === "blit-passphrase" && typeof data.passphrase === "string") {
    // Only the app holds the credential. A previewed page is same-origin
    // with it, so without this check any preview frame could post a bogus
    // passphrase: `setPassphrase` closes and clears the whole pool, and
    // leaves `authenticated` truthy so no re-auth is ever requested —
    // every preview 502s until the app is reloaded.
    if (senderPreview(event.source as Client | null)) return;
    pool.setPassphrase(data.passphrase);
    return;
  }
  // The bootstrap document naming its target. `event.source` is the client
  // itself, which is how the binding gets an id without anyone guessing one.
  if (data.type === "blit-ws-open" && data.target && event.ports[0]) {
    // `waitUntil`, not a bare call: a worker with no in-flight extendable
    // event is terminated when idle (~30s), and a relayed socket lives inside
    // the worker — so a long-lived WebSocket would die silently mid-session.
    // Holding the message event open for the socket's lifetime is what keeps
    // the worker alive while it is pumping. A browser may still impose its own
    // ceiling, which is why the close sentinel matters: the app is told, and
    // reconnects.
    event.waitUntil(pipeWebSocket(data.target, event.ports[0]));
    return;
  }
  if (data.type === "blit-cookie" && typeof data.value === "string") {
    // The jar comes from the sender's own target, not from the message: a
    // frame previewing one dev server must not be able to write cookies
    // into another's jar.
    const sender = senderPreview(event.source as Client | null);
    if (!sender) return;
    const key = previewKey(sender);
    let jar = jars.get(key);
    if (!jar) {
      jar = new CookieJar();
      jars.set(key, jar);
    }
    jar.set(data.value, "/");
    return;
  }
  if (data.type === "blit-bind" && data.target) {
    const source = event.source as Client | null;
    if (source?.id) {
      bindings.set(source.id, data.target);
      void rememberBinding(source.id, data.target);
    }
    event.ports[0]?.postMessage({ ok: true });
  }
});

self.addEventListener("fetch", (event: FetchEvent) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;

  const bootstrap =
    parsePreviewFrameUrl(url.pathname, url.search) ??
    parseBootstrapUrl(url.pathname, url.search);
  if (bootstrap) {
    // Bind only on a frame navigation. A plain `fetch()` of a preview URL is
    // still relayed — it is an explicit request for that target — but binding
    // its client would be catastrophic: the caller is usually the top-level
    // page, and a bound top-level client sends every one of the app's own
    // requests through the relay, which returns another origin's bytes for
    // them.
    const navigating =
      event.request.mode === "navigate" ||
      event.request.destination === "iframe";
    const id = navigating ? event.resultingClientId || event.clientId : "";
    if (id) {
      bindings.set(id, bootstrap.target);
      void rememberBinding(id, bootstrap.target);
    }
    event.respondWith(relay(event.request, bootstrap.target, bootstrap.path));
    return;
  }

  // Everything else: claim it only when it is certainly a preview's. Calling
  // respondWith for anything else would route the whole app through this
  // worker — every navigation and asset waiting on worker startup, and a bug
  // in here breaking the app rather than a pane. Returning without responding
  // leaves the browser's own path untouched.
  if (!isPreviewRequest(event)) return;
  event.respondWith(
    route(event).catch((err) =>
      problem(500, `preview worker failed: ${message(err)}`),
    ),
  );
});

/**
 * Synchronously decide whether a request could belong to a preview.
 *
 * Synchronous by necessity: `respondWith` must be called during dispatch, so
 * there is no awaiting `clients.get` before deciding. The in-memory bindings
 * answer for subresources; an iframe navigation is always claimed, since it is
 * either a bound frame moving or one whose binding needs recovering.
 */
function isPreviewRequest(event: FetchEvent): boolean {
  if (event.request.destination === "iframe") return true;
  const id = event.clientId || event.resultingClientId;
  return !!id && bindings.has(id);
}

/** Decide whether a request belongs to a preview. */
async function route(event: FetchEvent): Promise<Response> {
  // A navigation has no `clientId` — it is the client being created — so a
  // frame re-navigating within its target resolves through the id the
  // bootstrap bound.
  const target = await resolveTarget(event.clientId || event.resultingClientId);
  if (target) {
    const url = new URL(event.request.url);
    return relay(event.request, target, url.pathname + url.search);
  }
  // Only reachable for an iframe navigation we could not attribute.
  // An iframe navigating with no binding is a pane being opened: serve the
  // bootstrap document, which reads the target from the fragment, hands it
  // over, and replaces itself. A top-level navigation is the app and is never
  // touched — `destination` is what tells the two apart.
  if (event.request.destination === "iframe") {
    return bootstrapDocument();
  }
  return fetch(event.request);
}

async function resolveTarget(clientId: string): Promise<PreviewTarget | null> {
  if (!clientId) return null;
  await restored;
  const bound = bindings.get(clientId);
  // A navigation's resulting client does not exist yet, and `get` may reject
  // rather than resolve empty for an id it has never seen.
  const client = await self.clients.get(clientId).catch(() => undefined);
  // No client yet means a navigation in flight: only a binding can speak for
  // it, and one exists only if a bootstrap created it.
  if (!client) return bound ?? null;
  if (client.frameType !== "nested" && client.frameType !== "none") return null;
  if (bound) return bound;
  try {
    const url = new URL(client.url);
    const parsed = parseBootstrapUrl(url.pathname, url.search);
    if (parsed) {
      bindings.set(clientId, parsed.target);
      void rememberBinding(clientId, parsed.target);
      return parsed.target;
    }
  } catch {
    // Not a URL we can read.
  }
  return null;
}

/** Speak HTTP/1.1 to the target over a relayed socket. */
async function relay(
  request: Request,
  target: PreviewTarget,
  path: string,
): Promise<Response> {
  if (!pool.authenticated && !(await requestPassphrase())) {
    return problem(
      503,
      "No blit credential. Open the blit UI in a top-level tab, then reload this pane.",
    );
  }
  const key = previewKey(target);
  let jar = jars.get(key);
  if (!jar) {
    jar = new CookieJar();
    jars.set(key, jar);
  }

  const body = await requestBody(request);
  let stream;
  try {
    stream = await pool.open(target.dest, target.host, target.port, {
      tls: target.scheme === "https",
      // Offer only what we can speak.
      alpn: target.scheme === "https" ? ["http/1.1"] : undefined,
    });
    await stream.opened;
  } catch (err) {
    return problem(502, `Cannot reach ${describe(target)}: ${message(err)}`);
  }

  const host = target.host.includes(":") ? `[${target.host}]` : target.host;
  const authority =
    (target.scheme === "https" && target.port === 443) ||
    (target.scheme === "http" && target.port === 80)
      ? host
      : `${host}:${target.port}`;
  const head = encodeRequestHead({
    method: request.method,
    path,
    host: authority,
    headers: request.headers,
    contentLength: body ? body.length : undefined,
    cookie: jar.header(path),
    origin: `${target.scheme}://${authority}`,
    referer: request.referrer || undefined,
  });

  try {
    await stream.write(head);
    if (body && body.length > 0) await stream.write(body);
    // Half-close only when there is nothing more to send and the response is all we want: keep-alive requests must not shut the write side, or a pooled stream could not carry a second request.
    const source = stream.read();
    const { responseHead, prefix } = await readHead(source, request.method);
    for (const cookie of responseHead.setCookie) jar.set(cookie, path);
    const rewritten = rewriteHeaders(responseHead, target);
    let stream2 = bodyStream(responseHead, prefix, source, () =>
      stream.close(),
    );
    const isHtml = (rewritten.get("content-type") ?? "").includes("text/html");
    if (stream2 && isHtml && request.destination === "iframe") {
      // Only navigations, and only HTML: injecting into a fetched payload
      // would corrupt it.
      // `forScript`: the shim exposes this through `document.cookie`, so
      // HttpOnly entries must not be in it.
      stream2 = injectIntoHtml(stream2, target, jar.header(path, true) ?? "");
      // CSP is already dropped in rewriteHeaders, which the injected inline
      // script also depends on: a strict policy withholds `unsafe-inline`.
    }
    return new Response(stream2, {
      status: responseHead.status,
      statusText: responseHead.statusText,
      headers: rewritten,
    });
  } catch (err) {
    stream.close();
    return problem(502, `${describe(target)}: ${message(err)}`);
  }
}

/**
 * Relay a WebSocket as raw bytes between the page's shim and the target.
 *
 * Framing lives in the shim, not here: this end stays a byte pipe, which is
 * all the `NET` family is. The handshake bytes arrive from the shim like any
 * other payload.
 */
async function pipeWebSocket(
  target: PreviewTarget,
  port: MessagePort,
): Promise<void> {
  let stream;
  try {
    stream = await pool.open(target.dest, target.host, target.port, {
      tls: target.scheme === "https",
    });
    await stream.opened;
  } catch {
    // The sentinel matters most here: a refused connect must reach the shim, or
    // the socket sits in CONNECTING until its own timeout.
    try {
      port.postMessage({ blitClosed: true });
    } catch {
      // Port already unusable.
    }
    port.close();
    return;
  }
  // Serialize writes: `write` chunks and waits on credit, so two concurrent
  // calls can interleave their bytes and corrupt the frame stream.
  let queue: Promise<void> = Promise.resolve();
  port.onmessage = (event) => {
    const bytes = new Uint8Array(event.data as ArrayBuffer);
    queue = queue.then(() => stream.write(bytes)).catch(() => {});
  };
  try {
    for await (const chunk of stream.read()) {
      const copy = new Uint8Array(chunk);
      port.postMessage(copy.buffer, [copy.buffer]);
    }
  } catch {
    // Target closed or reset.
  }
  // A closed MessagePort fires no event on the other side, so the shim would
  // never learn the socket is dead — and an app that reconnects on close (every
  // HMR client) would hang instead. Say so explicitly before closing.
  try {
    port.postMessage({ blitClosed: true });
  } catch {
    // Port already unusable.
  }
  stream.close();
  port.close();
}

/** The target named by a referrer, when it is a preview URL. */
function fromReferrer(referrer: string): ParsedBootstrap | null {
  if (!referrer) return null;
  try {
    const url = new URL(referrer);
    if (url.origin !== self.location.origin) return null;
    return (
      parsePreviewFrameUrl(url.pathname, url.search) ??
      parseBootstrapUrl(url.pathname, url.search)
    );
  } catch {
    return null;
  }
}

/**
 * Ask any open page for the credential and wait briefly for it.
 *
 * A worker can be started or replaced at any moment, and one that merely waits
 * to be told answers 503 forever. Asking makes it self-healing: the page hands
 * the passphrase over on demand, and it is still never persisted here.
 */
async function requestPassphrase(): Promise<boolean> {
  const windows = await self.clients.matchAll({ type: "window" });
  if (windows.length === 0) return false;
  for (const client of windows) {
    client.postMessage({ type: "blit-need-passphrase" });
  }
  for (let waited = 0; waited < 2000; waited += 50) {
    if (pool.authenticated) return true;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return pool.authenticated;
}

/** Read until the response head is complete, keeping the body's first bytes. */
async function readHead(
  source: AsyncGenerator<Uint8Array, void, void>,
  method: string,
): Promise<{ responseHead: ResponseHead; prefix: Uint8Array }> {
  let buffer = new Uint8Array(0);
  for (;;) {
    const { done, value } = await source.next();
    if (done) throw new Error("closed before sending a response");
    const merged = new Uint8Array(buffer.length + value.length);
    merged.set(buffer, 0);
    merged.set(value, buffer.length);
    buffer = merged;
    const parsed = parseResponseHead(buffer, method);
    if (parsed) return { responseHead: parsed, prefix: parsed.rest };
    // A head this large is a broken target, not a slow one.
    if (buffer.length > 256 * 1024) {
      throw new Error("response head too large");
    }
  }
}

/** Fix up the headers the browser will act on. */
export function rewriteHeaders(
  head: ResponseHead,
  target: PreviewTarget,
): Headers {
  const headers = new Headers(head.headers);
  const location = headers.get("location");
  if (location) {
    // A redirect off the previewed origin is delivered unchanged, so the frame
    // follows it and leaves the relay — deliberate, because a dev server that
    // bounces you to an identity provider should still get you there. The
    // trade is worth stating: the browser resolves it directly, so a *remote*
    // target answering `Location: //localhost:9000` reaches the viewer's own
    // machine and not the server's.
    const rewritten = rewriteLocation(location, target);
    if (rewritten) headers.set("location", rewritten);
  }
  // Content-Length would contradict the stream we actually deliver (chunked decoded, or truncated); the browser computes what it needs.
  headers.delete("content-length");
  // A preview must not claim authority over the gateway origin's other paths.
  headers.delete("clear-site-data");
  headers.delete("service-worker-allowed");
  // Framing refusals are enforced by the browser against *this* response, and
  // this response is ours — so dropping them is all it takes to preview a site
  // that says it does not want to be framed. Deliberate: those headers are a
  // site's clickjacking defence, and removing them is only defensible because a
  // preview is the operator looking at their own target on their own screen.
  headers.delete("x-frame-options");
  headers.delete("content-security-policy");
  headers.delete("content-security-policy-report-only");
  return headers;
}

export function rewriteLocation(
  location: string,
  target: PreviewTarget,
): string | null {
  // `//host/path` is an authority, not a path, so it must not be taken for a
  // clean same-origin one. Resolved against the target's scheme instead: one
  // naming the target becomes a path and the frame stays in the preview, and
  // one naming anything else is left to the browser like any other off-target
  // redirect. Treating it as a path sent even an on-target `//host` straight
  // out of the relay.
  if (location.startsWith("//")) {
    return targetPath(`${target.scheme}:${location}`, target);
  }
  if (location.startsWith("/")) return location; // already clean-path
  return targetPath(location, target);
}

/**
 * `absolute` as a path on `target`, or null when it names somewhere else.
 *
 * Null means the header is delivered unchanged and the browser follows it out
 * of the relay — see `rewriteHeaders`.
 */
function targetPath(absolute: string, target: PreviewTarget): string | null {
  let url: URL;
  try {
    url = new URL(absolute);
  } catch {
    return null;
  }
  const sameHost =
    url.hostname.replace(/^\[|\]$/g, "") === target.host &&
    (url.port ? Number(url.port) : url.protocol === "https:" ? 443 : 80) ===
      target.port;
  return sameHost ? url.pathname + url.search + url.hash : null;
}

async function requestBody(request: Request): Promise<Uint8Array | null> {
  if (request.method === "GET" || request.method === "HEAD") return null;
  const buffer = await request.clone().arrayBuffer();
  return buffer.byteLength > 0 ? new Uint8Array(buffer) : null;
}

function describe(target: PreviewTarget): string {
  return `${target.scheme}://${target.host}:${target.port} on ${target.dest}`;
}

function message(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/** A legible failure. */
function problem(status: number, text: string): Response {
  return new Response(`blit preview: ${text}\n`, {
    status,
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "no-store",
    },
  });
}

export { PREVIEW_PREFIX };
