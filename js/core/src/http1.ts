/** HTTP/1.1 over a relayed byte stream (docs/design/net.md § Client: service worker). */

const encoder = new TextEncoder();

/** Headers a proxying client must never forward verbatim. */
const HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
  // The browser's own cookie handling is per-origin, and every relayed target shares one origin — so a proxying client owns cookies itself and must not pass the browser's header through (docs/design/net.md § Clean paths).
  "cookie",
  // Set by us from the target, never from the incoming request.
  "host",
  // The browser stamps these with the *gateway's* origin, and a target that
  // checks Origin against Host — every server-function/CSRF guard does, e.g.
  // TanStack Start, Next server actions — rejects that as cross-site. Rewritten
  // below to the target's own origin.
  "origin",
  "referer",
]);

export interface RequestHeadOptions {
  method: string;
  /** Origin-form request target: path plus query. */
  path: string;
  /** `Host` header value — the target's, never the gateway's. */
  host: string;
  headers: Headers;
  /** Body length when known; omitted means no body. */
  contentLength?: number;
  /** Cookies to present, already serialized as `a=1; b=2`. */
  cookie?: string;
  /** The target's own origin (`http://host:port`), used for `Origin` and to
   *  rebase `Referer`. Omit to send neither. */
  origin?: string;
  /** The incoming `Referer`, rebased onto `origin` when both are present. */
  referer?: string;
}

/** Serialize a request head. */
export function encodeRequestHead(options: RequestHeadOptions): Uint8Array {
  const lines: string[] = [`${options.method} ${options.path} HTTP/1.1`];
  lines.push(`Host: ${options.host}`);
  for (const [name, value] of options.headers) {
    if (HOP_BY_HOP.has(name.toLowerCase())) continue;
    lines.push(`${name}: ${value}`);
  }
  if (options.origin) {
    lines.push(`Origin: ${options.origin}`);
    if (options.referer) {
      try {
        const from = new URL(options.referer);
        lines.push(`Referer: ${options.origin}${from.pathname}${from.search}`);
      } catch {
        // Unparseable referrer: better none than the gateway's.
      }
    }
  }
  if (options.cookie) lines.push(`Cookie: ${options.cookie}`);
  if (options.contentLength !== undefined) {
    lines.push(`Content-Length: ${options.contentLength}`);
  }
  lines.push("Connection: close");
  return encoder.encode(lines.join("\r\n") + "\r\n\r\n");
}

export interface ResponseHead {
  status: number;
  statusText: string;
  headers: Headers;
  /** Every `Set-Cookie` value, kept separate — `Headers` folds duplicates and a folded cookie header is not recoverable. */
  setCookie: string[];
  /** How the body ends. */
  framing: "length" | "chunked" | "eof" | "none";
  contentLength: number;
  /** Bytes already read past the head, the start of the body. */
  rest: Uint8Array;
}

/** Status codes that never carry a body, whatever the headers claim. */
function bodyless(status: number, method: string): boolean {
  return (
    method === "HEAD" ||
    status === 204 ||
    status === 304 ||
    (status >= 100 && status < 200)
  );
}

/** Parse a response head out of `buffer`, or `null` when more bytes are needed. */
export function parseResponseHead(
  buffer: Uint8Array,
  method: string,
): ResponseHead | null {
  const end = findHeadEnd(buffer);
  if (end < 0) return null;
  const text = new TextDecoder("utf-8").decode(
    buffer.subarray(0, end.valueOf()),
  );
  const lines = text.split("\r\n").filter((l) => l.length > 0);
  if (lines.length === 0) throw new Error("empty response head");
  const statusLine = lines[0];
  const match = /^HTTP\/1\.[01] (\d{3})(?: (.*))?$/.exec(statusLine);
  if (!match) throw new Error(`bad status line: ${statusLine.slice(0, 64)}`);
  const status = Number(match[1]);
  const statusText = match[2] ?? "";
  const headers = new Headers();
  const setCookie: string[] = [];
  let contentLength = -1;
  let chunked = false;
  for (const line of lines.slice(1)) {
    const colon = line.indexOf(":");
    if (colon <= 0) continue;
    const name = line.slice(0, colon).trim();
    const value = line.slice(colon + 1).trim();
    const lower = name.toLowerCase();
    if (lower === "set-cookie") {
      setCookie.push(value);
      continue;
    }
    if (lower === "content-length") {
      const n = Number(value);
      if (Number.isFinite(n) && n >= 0) contentLength = n;
      continue;
    }
    if (lower === "transfer-encoding") {
      chunked = value.toLowerCase().includes("chunked");
      continue;
    }
    if (lower === "connection" || lower === "keep-alive") continue;
    try {
      headers.append(name, value);
    } catch {
      // A header name the Headers guard rejects is dropped rather than failing the whole response — the body is still worth delivering.
    }
  }
  let framing: ResponseHead["framing"];
  if (bodyless(status, method)) {
    framing = "none";
    // `Content-Length` on a 304 describes the resource, not this message.
    contentLength = 0;
  } else if (chunked) {
    framing = "chunked";
  } else if (contentLength >= 0) {
    framing = "length";
  } else {
    // No length and no chunking: the body ends when the stream does, which is why a relay must surface EOF rather than treat it as an error.
    framing = "eof";
  }
  return {
    status,
    statusText,
    headers,
    setCookie,
    framing,
    contentLength: contentLength < 0 ? 0 : contentLength,
    rest: buffer.subarray(end.valueOf() + 4),
  };
}

/** Index of the CRLFCRLF that ends the head, or -1. */
function findHeadEnd(buffer: Uint8Array): number {
  for (let i = 0; i + 3 < buffer.length; i++) {
    if (
      buffer[i] === 13 &&
      buffer[i + 1] === 10 &&
      buffer[i + 2] === 13 &&
      buffer[i + 3] === 10
    ) {
      return i;
    }
  }
  return -1;
}

/** Decode a chunked body incrementally. */
export class ChunkedDecoder {
  private buffer: Uint8Array<ArrayBuffer> = new Uint8Array(0);
  private remaining = 0;
  private state: "size" | "data" | "crlf" | "trailer" | "done" = "size";

  get done(): boolean {
    return this.state === "done";
  }

  push(bytes: Uint8Array): Uint8Array<ArrayBuffer>[] {
    this.buffer = concat(this.buffer, bytes);
    const out: Uint8Array<ArrayBuffer>[] = [];
    for (;;) {
      if (this.state === "done") return out;
      if (this.state === "size") {
        const nl = indexOfCrlf(this.buffer);
        if (nl < 0) return out;
        const line = new TextDecoder().decode(this.buffer.subarray(0, nl));
        this.buffer = this.buffer.subarray(nl + 2);
        // Chunk extensions after ";" are legal and ignorable.
        const size = parseInt(line.split(";")[0].trim(), 16);
        if (!Number.isFinite(size) || size < 0) {
          throw new Error(`bad chunk size: ${line.slice(0, 32)}`);
        }
        if (size === 0) {
          this.state = "trailer";
          continue;
        }
        this.remaining = size;
        this.state = "data";
        continue;
      }
      if (this.state === "data") {
        if (this.buffer.length === 0) return out;
        const take = Math.min(this.remaining, this.buffer.length);
        out.push(this.buffer.subarray(0, take) as Uint8Array<ArrayBuffer>);
        this.buffer = this.buffer.subarray(take);
        this.remaining -= take;
        if (this.remaining === 0) this.state = "crlf";
        continue;
      }
      if (this.state === "crlf") {
        if (this.buffer.length < 2) return out;
        this.buffer = this.buffer.subarray(2);
        this.state = "size";
        continue;
      }
      // Trailer section: headers until a bare CRLF.
      const nl = indexOfCrlf(this.buffer);
      if (nl < 0) return out;
      const line = this.buffer.subarray(0, nl);
      this.buffer = this.buffer.subarray(nl + 2);
      if (line.length === 0) this.state = "done";
    }
  }
}

function indexOfCrlf(buffer: Uint8Array): number {
  for (let i = 0; i + 1 < buffer.length; i++) {
    if (buffer[i] === 13 && buffer[i + 1] === 10) return i;
  }
  return -1;
}

export function concat(a: Uint8Array, b: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

/** Turn a stream of body bytes into a `ReadableStream`, respecting the head's framing. */
export function bodyStream(
  head: ResponseHead,
  prefix: Uint8Array,
  source: AsyncGenerator<Uint8Array, void, void>,
  /** Called once the body has ended, however it ended — the hook that retires
   *  the underlying socket instead of stranding it. */
  onDone?: () => void,
): ReadableStream<Uint8Array<ArrayBuffer>> | null {
  if (head.framing === "none") {
    onDone?.();
    return null;
  }
  if (head.framing === "chunked") {
    const decoder = new ChunkedDecoder();
    return new ReadableStream<Uint8Array<ArrayBuffer>>({
      async start(controller) {
        try {
          for (const piece of decoder.push(prefix)) controller.enqueue(piece);
          if (!decoder.done) {
            for await (const bytes of source) {
              for (const piece of decoder.push(bytes)) {
                controller.enqueue(piece);
              }
              if (decoder.done) break;
            }
          }
          controller.close();
        } catch (err) {
          controller.error(err);
        } finally {
          onDone?.();
        }
      },
    });
  }
  const limit = head.framing === "length" ? head.contentLength : Infinity;
  return new ReadableStream<Uint8Array<ArrayBuffer>>({
    async start(controller) {
      let delivered = 0;
      const emit = (bytes: Uint8Array): boolean => {
        const take = Math.min(bytes.length, limit - delivered);
        if (take > 0) {
          controller.enqueue(
            new Uint8Array(bytes.subarray(0, take)) as Uint8Array<ArrayBuffer>,
          );
          delivered += take;
        }
        return delivered >= limit;
      };
      try {
        if (!emit(prefix)) {
          for await (const bytes of source) {
            if (emit(bytes)) break;
          }
        }
        // A short `Content-Length` body is a truncated response, and saying so beats handing the page half a script it will fail on obscurely.
        if (head.framing === "length" && delivered < limit) {
          controller.error(
            new Error(`truncated body: ${delivered} of ${limit} bytes`),
          );
          return;
        }
        controller.close();
      } catch (err) {
        controller.error(err);
      } finally {
        onDone?.();
      }
    },
  });
}
