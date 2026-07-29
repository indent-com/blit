import { describe, expect, it } from "vitest";
import {
  formatPreviewLocation,
  looksLikeWebLocation,
  parseBootstrapUrl,
  parsePreviewFrameUrl,
  parsePreviewLocation,
  previewBootstrapUrl,
  previewFrameUrl,
  previewKey,
} from "../preview";

describe("parsePreviewLocation", () => {
  it("takes a full origin", () => {
    expect(parsePreviewLocation("https://localhost:3000", "local")).toEqual({
      dest: "local",
      scheme: "https",
      host: "localhost",
      port: 3000,
    });
  });

  it("defaults a bare host:port to http, which is what people type", () => {
    expect(parsePreviewLocation("localhost:3000", "local")).toEqual({
      dest: "local",
      scheme: "http",
      host: "localhost",
      port: 3000,
    });
  });

  it("fills in the scheme's default port", () => {
    expect(parsePreviewLocation("https://api.internal", "prod").port).toBe(443);
    expect(parsePreviewLocation("http://api.internal", "prod").port).toBe(80);
  });

  it("unbrackets IPv6, because the wire wants the bare address", () => {
    expect(parsePreviewLocation("http://[fd00::5]:8080", "local")).toEqual({
      dest: "local",
      scheme: "http",
      host: "fd00::5",
      port: 8080,
    });
  });

  it("refuses what is not an http(s) origin", () => {
    for (const bad of ["ws://h:80", "file:///etc/passwd", "", "http://"]) {
      expect(() => parsePreviewLocation(bad, "local")).toThrow();
    }
  });

  it("round-trips through its display form", () => {
    for (const location of [
      "http://localhost:3000",
      "https://api.internal",
      "http://[fd00::5]:8080",
    ]) {
      const target = parsePreviewLocation(location, "local");
      expect(formatPreviewLocation(target)).toBe(location);
      expect(
        parsePreviewLocation(formatPreviewLocation(target), "local"),
      ).toEqual(target);
    }
  });

  it("hides the implicit port in the display form", () => {
    expect(
      formatPreviewLocation({
        dest: "d",
        scheme: "https",
        host: "h",
        port: 443,
      }),
    ).toBe("https://h");
    expect(
      formatPreviewLocation({
        dest: "d",
        scheme: "https",
        host: "h",
        port: 8443,
      }),
    ).toBe("https://h:8443");
  });
});

describe("bootstrap URLs", () => {
  it("round-trips a target and path", () => {
    const target = parsePreviewLocation("https://localhost:3000", "local");
    const url = previewBootstrapUrl(target, "/dashboard");
    expect(url).toBe("/x/local/https/localhost:3000/dashboard");
    expect(
      parseBootstrapUrl("/x/local/https/localhost:3000/dashboard"),
    ).toEqual({ target, path: "/dashboard" });
  });

  it("keeps the query string on the target's side", () => {
    expect(parseBootstrapUrl("/x/local/http/h:80/search", "?q=1")?.path).toBe(
      "/search?q=1",
    );
  });

  it("round-trips a bracketed IPv6 target", () => {
    const target = parsePreviewLocation("http://[fd00::5]:8080", "local");
    const url = previewBootstrapUrl(target);
    expect(parseBootstrapUrl(url)).toEqual({ target, path: "/" });
  });

  it("survives a destination name needing escaping", () => {
    const target = parsePreviewLocation("http://h:80", "my remote/1");
    const url = previewBootstrapUrl(target);
    expect(parseBootstrapUrl(url)?.target.dest).toBe("my remote/1");
  });

  it("returns null for anything that is not a bootstrap URL", () => {
    for (const bad of [
      "/",
      "/index.html",
      "/x/",
      "/x/local",
      "/x/local/ftp/h:80/",
      "/x/local/http/noport/",
      "/x//http/h:80/",
    ]) {
      expect(parseBootstrapUrl(bad)).toBeNull();
    }
  });

  it("keys pooling and cookies by the whole origin", () => {
    const a = parsePreviewLocation("http://localhost:3000", "local");
    const b = parsePreviewLocation("http://localhost:3001", "local");
    const c = parsePreviewLocation("http://localhost:3000", "prod");
    expect(previewKey(a)).not.toBe(previewKey(b));
    expect(previewKey(a)).not.toBe(previewKey(c));
    expect(previewKey(a)).toBe(previewKey({ ...a }));
  });
});

describe("frame URLs", () => {
  it("keeps pathname clean and puts the target in the query", () => {
    // What a client-side router inside the frame reads is `pathname`; if that
    // carried a proxy prefix the app would route on it.
    const target = parsePreviewLocation("http://localhost:3000", "local");
    const url = previewFrameUrl(target);
    expect(new URL(url, "http://gw").pathname).toBe("/");
    expect(parsePreviewFrameUrl("/", new URL(url, "http://gw").search)).toEqual(
      {
        target,
        path: "/",
      },
    );
  });

  it("carries an initial path without dirtying the pathname", () => {
    const target = parsePreviewLocation("https://api.internal", "prod");
    const url = previewFrameUrl(target, "/dashboard?tab=1");
    const parsed = new URL(url, "http://gw");
    expect(parsed.pathname).toBe("/");
    expect(parsePreviewFrameUrl("/", parsed.search)).toEqual({
      target,
      path: "/dashboard?tab=1",
    });
  });

  it("never collides with the app's own root", () => {
    // A frame whose URL equals an ancestor's is refused as recursive nesting,
    // so the query must always be present.
    const url = previewFrameUrl(parsePreviewLocation("http://h:80", "local"));
    expect(url).not.toBe("/");
    expect(url.startsWith("/?")).toBe(true);
  });

  it("ignores a root URL that is not a preview", () => {
    expect(parsePreviewFrameUrl("/", "")).toBeNull();
    expect(parsePreviewFrameUrl("/", "?other=1")).toBeNull();
    expect(
      parsePreviewFrameUrl("/app", "?blit-preview=local|http|h|80"),
    ).toBeNull();
    expect(
      parsePreviewFrameUrl("/", "?blit-preview=local|ftp|h|80"),
    ).toBeNull();
    expect(
      parsePreviewFrameUrl("/", "?blit-preview=local|http|h|0"),
    ).toBeNull();
  });
});

describe("looksLikeWebLocation", () => {
  it("accepts a scheme or a host:port", () => {
    for (const value of [
      "http://localhost:3000",
      "https://api.internal",
      "localhost:3000",
      "127.0.0.1:8080",
      "[fd00::5]:8080",
      "localhost:3000/dashboard",
    ]) {
      expect(looksLikeWebLocation(value)).toBe(true);
    }
  });

  it("leaves commands alone", () => {
    // A bare word is a program name; guessing otherwise would swallow a
    // terminal the user asked for.
    for (const value of [
      "htop",
      "localhost",
      "vim src/main.rs",
      "npm run dev",
      "",
      "  ",
      "ssh host",
      "make -j8",
    ]) {
      expect(looksLikeWebLocation(value)).toBe(false);
    }
  });
});

describe("host validation", () => {
  // Every parser here decodeURIComponent's its input, and the host it
  // returns goes straight into the upstream Host:/Origin: headers. A parser
  // that accepts a newline inside a hostname lies about what it returns.
  const hostile = [
    "evil%0d%0aX-Injected:%201",
    "evil%0a",
    "evil%20host",
    "evil%00",
  ];

  it("rejects control characters in a frame URL's host", () => {
    for (const host of hostile) {
      const spec = `local|http|${host}|3000`;
      expect(
        parsePreviewFrameUrl("/", `?blit-preview=${encodeURIComponent(spec)}`),
        host,
      ).toBeNull();
    }
    // The clean form still parses.
    expect(
      parsePreviewFrameUrl(
        "/",
        `?blit-preview=${encodeURIComponent("local|http|localhost|3000")}`,
      )?.target.host,
    ).toBe("localhost");
  });

  it("rejects control characters in a bootstrap URL's host", () => {
    // A bootstrap pathname's authority is never decoded, so a percent
    // escape stays inert here; a literal control character is what would
    // reach the upstream header, and that is what must be refused.
    for (const host of ["evil\r\nX-Injected: 1", "evil\n", "evil host"]) {
      expect(parseBootstrapUrl(`/x/local/http/${host}:3000/`), host).toBeNull();
    }
    expect(
      parseBootstrapUrl("/x/local/http/localhost:3000/")?.target.host,
    ).toBe("localhost");
  });

  it("keeps a typed location's host free of control characters", () => {
    // WHATWG URL parsing strips tab/newline from its input rather than
    // failing, so this yields a clean host rather than throwing — the point
    // is that no control character survives into the target either way.
    expect(parsePreviewLocation("http://evil\nhost:3000", "d").host).toBe(
      "evilhost",
    );
    // IPv6 literals are the one host that legitimately contains colons.
    expect(parsePreviewLocation("http://[::1]:3000", "d").host).toBe("::1");
  });
});
