import { describe, expect, it } from "vitest";
import {
  ChunkedDecoder,
  bodyStream,
  encodeRequestHead,
  parseResponseHead,
} from "../http1";

const enc = new TextEncoder();
const dec = new TextDecoder();

async function* feed(...chunks: string[]): AsyncGenerator<Uint8Array> {
  for (const chunk of chunks) yield enc.encode(chunk);
}

async function drain(stream: ReadableStream<Uint8Array> | null) {
  if (!stream) return null;
  const reader = stream.getReader();
  let out = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    out += dec.decode(value);
  }
  return out;
}

describe("encodeRequestHead", () => {
  it("writes the target's Host and asks the target to close", () => {
    const head = dec.decode(
      encodeRequestHead({
        method: "GET",
        path: "/assets/app.js?v=2",
        host: "localhost:3000",
        headers: new Headers({ accept: "*/*" }),
      }),
    );
    expect(head.split("\r\n")[0]).toBe("GET /assets/app.js?v=2 HTTP/1.1");
    expect(head).toContain("Host: localhost:3000");
    expect(head).toContain("accept: */*");
    // One socket per request, closed when the body ends: asking to keep it
    // alive without a pool is how the socket budget gets exhausted.
    expect(head).toContain("Connection: close");
    expect(head.endsWith("\r\n\r\n")).toBe(true);
  });

  it("drops hop-by-hop headers and the browser's cookies", () => {
    // Every relayed target shares one origin, so the browser's Cookie header
    // belongs to no target in particular — the worker owns cookies instead.
    const head = dec.decode(
      encodeRequestHead({
        method: "GET",
        path: "/",
        host: "h:80",
        headers: new Headers({
          cookie: "session=leaky",
          connection: "close",
          "transfer-encoding": "chunked",
          host: "gateway.example",
          "x-keep": "yes",
        }),
        cookie: "session=ours",
      }),
    );
    expect(head).not.toContain("session=leaky");
    expect(head).not.toContain("gateway.example");
    // The incoming Connection header is not forwarded; the only one present is
    // the one we write ourselves.
    expect(head.match(/^Connection:/gim)?.length).toBe(1);
    expect(head).toContain("Cookie: session=ours");
    expect(head).toContain("x-keep: yes");
  });

  it("declares a body length when there is one", () => {
    const head = dec.decode(
      encodeRequestHead({
        method: "POST",
        path: "/api",
        host: "h:80",
        headers: new Headers(),
        contentLength: 12,
      }),
    );
    expect(head).toContain("Content-Length: 12");
  });
});

describe("parseResponseHead", () => {
  it("returns null until the head is complete", () => {
    const partial = enc.encode("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n");
    expect(parseResponseHead(partial, "GET")).toBeNull();
  });

  it("parses status, headers and the start of the body", () => {
    const head = parseResponseHead(
      enc.encode(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
      ),
      "GET",
    )!;
    expect(head.status).toBe(200);
    expect(head.statusText).toBe("OK");
    expect(head.headers.get("content-type")).toBe("text/plain");
    expect(head.framing).toBe("length");
    expect(head.contentLength).toBe(5);
    expect(dec.decode(head.rest)).toBe("hello");
  });

  it("keeps every Set-Cookie separately", () => {
    // Headers folds duplicates with ", " and a folded cookie header cannot be
    // split again — Expires dates contain commas.
    const head = parseResponseHead(
      enc.encode(
        "HTTP/1.1 200 OK\r\n" +
          "Set-Cookie: a=1; Path=/; Expires=Wed, 21 Oct 2026 07:28:00 GMT\r\n" +
          "Set-Cookie: b=2; HttpOnly\r\n\r\n",
      ),
      "GET",
    )!;
    expect(head.setCookie).toHaveLength(2);
    expect(head.setCookie[0]).toContain("a=1");
    expect(head.setCookie[1]).toContain("b=2");
    expect(head.headers.get("set-cookie")).toBeNull();
  });

  it("recognizes chunked framing", () => {
    const head = parseResponseHead(
      enc.encode("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"),
      "GET",
    )!;
    expect(head.framing).toBe("chunked");
  });

  it("falls back to EOF framing when nothing says otherwise", () => {
    const head = parseResponseHead(
      enc.encode("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n"),
      "GET",
    )!;
    expect(head.framing).toBe("eof");
  });

  it("gives 204, 304 and HEAD no body", () => {
    for (const [status, method] of [
      [204, "GET"],
      [304, "GET"],
      [200, "HEAD"],
    ] as const) {
      const head = parseResponseHead(
        enc.encode(`HTTP/1.1 ${status} X\r\nContent-Length: 99\r\n\r\n`),
        method,
      )!;
      expect(head.framing).toBe("none");
      expect(bodyStream(head, new Uint8Array(0), feed())).toBeNull();
    }
  });

  it("throws on a bogus status line rather than inventing one", () => {
    expect(() =>
      parseResponseHead(enc.encode("NOT HTTP AT ALL\r\n\r\n"), "GET"),
    ).toThrow(/bad status line/);
  });
});

describe("ChunkedDecoder", () => {
  it("decodes chunks split arbitrarily across pushes", () => {
    const decoder = new ChunkedDecoder();
    const out: string[] = [];
    // "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n", one byte at a time.
    const wire = "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
    for (const ch of wire) {
      for (const piece of decoder.push(enc.encode(ch))) {
        out.push(dec.decode(piece));
      }
    }
    expect(out.join("")).toBe("hello world");
    expect(decoder.done).toBe(true);
  });

  it("ignores chunk extensions and trailers", () => {
    const decoder = new ChunkedDecoder();
    const pieces = decoder.push(
      enc.encode("5;foo=bar\r\nhello\r\n0\r\nX-Trailer: 1\r\n\r\n"),
    );
    expect(pieces.map((p) => dec.decode(p)).join("")).toBe("hello");
    expect(decoder.done).toBe(true);
  });

  it("throws on a bad chunk size", () => {
    const decoder = new ChunkedDecoder();
    expect(() => decoder.push(enc.encode("zz\r\n"))).toThrow(/bad chunk size/);
  });
});

describe("bodyStream", () => {
  it("streams a length-delimited body and stops at the limit", async () => {
    const head = parseResponseHead(
      enc.encode("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhel"),
      "GET",
    )!;
    const body = bodyStream(head, head.rest, feed("lo wo", "rld", "EXTRA"));
    expect(await drain(body)).toBe("hello world");
  });

  it("errors on a body shorter than Content-Length", async () => {
    // Half a script delivered as if whole fails obscurely later; say it here.
    const head = parseResponseHead(
      enc.encode("HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n"),
      "GET",
    )!;
    const body = bodyStream(head, head.rest, feed("short"));
    await expect(drain(body)).rejects.toThrow(/truncated body: 5 of 20/);
  });

  it("streams an EOF-framed body until the source ends", async () => {
    const head = parseResponseHead(
      enc.encode("HTTP/1.1 200 OK\r\n\r\n<html>"),
      "GET",
    )!;
    expect(
      await drain(bodyStream(head, head.rest, feed("body", "</html>"))),
    ).toBe("<html>body</html>");
  });

  it("streams a chunked body that starts inside the head buffer", async () => {
    const head = parseResponseHead(
      enc.encode(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n",
      ),
      "GET",
    )!;
    const body = bodyStream(head, head.rest, feed("3\r\n hi\r\n0\r\n\r\n"));
    expect(await drain(body)).toBe("hello hi");
  });
});

describe("origin rewriting", () => {
  it("presents the target's origin, not the gateway's", () => {
    // A target that checks Origin against Host rejects the gateway's origin as
    // cross-site — which is how server-function handlers 403.
    const head = new TextDecoder().decode(
      encodeRequestHead({
        method: "POST",
        path: "/_serverFn/x",
        host: "localhost:4100",
        headers: new Headers({
          origin: "http://localhost:10000",
          referer: "http://localhost:10000/dashboard?a=1",
        }),
        origin: "http://localhost:4100",
        referer: "http://localhost:10000/dashboard?a=1",
      }),
    );
    expect(head).toContain("Origin: http://localhost:4100");
    expect(head).toContain("Referer: http://localhost:4100/dashboard?a=1");
    expect(head).not.toContain("localhost:10000");
  });

  it("sends neither header when no origin is given", () => {
    const head = new TextDecoder().decode(
      encodeRequestHead({
        method: "GET",
        path: "/",
        host: "h:80",
        headers: new Headers({ origin: "http://gw", referer: "http://gw/x" }),
      }),
    );
    expect(head).not.toMatch(/^Origin:/im);
    expect(head).not.toMatch(/^Referer:/im);
  });
});
