import { describe, expect, it } from "vitest";

import {
  assessUrl,
  escapeUrlForDisplay,
  openUrlSafely,
} from "../urlSecurity.js";

describe("assessUrl: allowed", () => {
  it.each([
    "https://blit.sh",
    "https://blit.sh/docs/protocol#cells",
    "http://localhost:3264/",
    "https://example.com/path?q=1&r=2",
    "mailto:someone@example.com",
  ])("allows %s", (url) => {
    expect(assessUrl(url).verdict).toBe("allow");
  });

  it("keeps the raw target intact for allowed URLs", () => {
    const a = assessUrl("https://blit.sh/a%20b?x=1");
    expect(a.raw).toBe("https://blit.sh/a%20b?x=1");
    expect(a.display).toBe("https://blit.sh/a%20b?x=1");
  });
});

describe("assessUrl: denied schemes", () => {
  it.each([
    "javascript:alert(1)",
    "JaVaScRiPt:alert(1)",
    "data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==",
    "vbscript:msgbox(1)",
    "blob:https://example.com/1234",
    "about:blank",
    "view-source:https://example.com",
    "chrome://settings",
    "filesystem:https://example.com/temporary/x",
  ])("denies %s", (url) => {
    const a = assessUrl(url);
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("dangerous-scheme");
  });

  it("denies a scheme obfuscated with percent-encoding", () => {
    // Percent-encoding is not valid in a scheme, so this has no scheme at all
    // rather than being a sneaky `javascript:`.
    const a = assessUrl("%6Aavascript:alert(1)");
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("no-scheme");
  });

  it("denies a scheme hidden behind a leading control character", () => {
    // `new URL()` strips these before parsing, which is how this bypasses a
    // naive scheme check built on top of it.
    const a = assessUrl("\u0001javascript:alert(1)");
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("hidden-characters");
  });

  it("denies a scheme split by a newline", () => {
    const a = assessUrl("java\nscript:alert(1)");
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("hidden-characters");
  });
});

describe("assessUrl: hidden characters", () => {
  it("denies a right-to-left override", () => {
    // Renders as though the host were something else entirely.
    const a = assessUrl("https://example.com/\u202Egnp.exe");
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("hidden-characters");
  });

  it.each([
    ["zero-width space", "https://exa\u200Bmple.com"],
    ["zero-width joiner", "https://exa\u200Dmple.com"],
    ["soft hyphen", "https://exa\u00ADmple.com"],
    ["BOM", "https://example.com\uFEFF"],
    ["bidi isolate", "https://\u2066example.com\u2069"],
    ["tag character", "https://example.com\u{E0041}"],
    ["no-break space", "https://example.com/a\u00A0b"],
  ])("denies %s", (_label, url) => {
    const a = assessUrl(url);
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("hidden-characters");
  });

  it("denies trailing spaces used to push the real target out of view", () => {
    const a = assessUrl(`https://safe.example${" ".repeat(200)}@evil.example`);
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("hidden-characters");
  });
});

describe("assessUrl: look-alike destinations", () => {
  it("asks about credentials hiding the real host", () => {
    const a = assessUrl("https://www.your-bank.example@evil.example/login");
    expect(a.verdict).toBe("confirm");
    expect(a.reason).toBe("embedded-credentials");
    // The dialog must name where the click actually goes.
    expect(a.detail).toContain("evil.example");
  });

  it("asks about punycode hosts", () => {
    const a = assessUrl("https://xn--80ak6aa92e.com/");
    expect(a.verdict).toBe("confirm");
    expect(a.reason).toBe("deceptive-host");
  });

  it("asks about non-ASCII hosts", () => {
    const a = assessUrl("https://\u0430pple.com/"); // Cyrillic \u0430
    expect(a.verdict).toBe("confirm");
    expect(a.reason).toBe("deceptive-host");
  });
});

describe("assessUrl: file and custom schemes", () => {
  it("denies remote file URLs", () => {
    const a = assessUrl("file://attacker.example/share/payload");
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("remote-file");
  });

  it("asks about local file URLs", () => {
    const a = assessUrl("file:///home/user/report.pdf");
    expect(a.verdict).toBe("confirm");
    expect(a.reason).toBe("local-file");
  });

  it("treats file://localhost as local", () => {
    expect(assessUrl("file://localhost/etc/hosts").reason).toBe("local-file");
  });

  it("asks about custom schemes", () => {
    for (const url of [
      "slack://channel?id=1",
      "vscode://file/tmp/x",
      "ssh://host",
    ]) {
      const a = assessUrl(url);
      expect(a.verdict).toBe("confirm");
      expect(a.reason).toBe("custom-scheme");
    }
  });
});

describe("assessUrl: degenerate input", () => {
  it("denies an empty target", () => {
    expect(assessUrl("").reason).toBe("empty");
  });

  it("denies a relative target", () => {
    expect(assessUrl("/etc/passwd").reason).toBe("no-scheme");
    expect(assessUrl("example.com").reason).toBe("no-scheme");
  });

  it("denies an over-long target without echoing all of it", () => {
    const a = assessUrl("https://example.com/" + "a".repeat(9000));
    expect(a.verdict).toBe("deny");
    expect(a.reason).toBe("too-long");
    expect(a.display.length).toBeLessThanOrEqual(4096);
  });
});

describe("escapeUrlForDisplay", () => {
  it("makes invisible characters visible", () => {
    expect(escapeUrlForDisplay("a\u202Eb")).toBe("a<U+202E>b");
    expect(escapeUrlForDisplay("a\u200Bb")).toBe("a<U+200B>b");
    expect(escapeUrlForDisplay("a\tb")).toBe("a<U+0009>b");
  });

  it("leaves ordinary URLs untouched", () => {
    expect(escapeUrlForDisplay("https://blit.sh/x?y=1")).toBe(
      "https://blit.sh/x?y=1",
    );
  });

  it("handles astral-plane codepoints without splitting surrogates", () => {
    expect(escapeUrlForDisplay("a\u{E0041}b")).toBe("a<U+E0041>b");
    expect(escapeUrlForDisplay("emoji\u{1F600}")).toBe("emoji\u{1F600}");
  });
});

describe("openUrlSafely", () => {
  it("opens allowed URLs without prompting", () => {
    const opened: string[] = [];
    expect(openUrlSafely("https://blit.sh", (u) => opened.push(u))).toBe(true);
    expect(opened).toEqual(["https://blit.sh"]);
  });

  it("never opens a denied URL", () => {
    const opened: string[] = [];
    const alert = globalThis.alert;
    globalThis.alert = () => {};
    try {
      expect(openUrlSafely("javascript:alert(1)", (u) => opened.push(u))).toBe(
        false,
      );
    } finally {
      globalThis.alert = alert;
    }
    expect(opened).toEqual([]);
  });

  it("respects a declined confirmation", () => {
    const opened: string[] = [];
    const confirm = globalThis.confirm;
    globalThis.confirm = () => false;
    try {
      expect(
        openUrlSafely("file:///tmp/payload.sh", (u) => opened.push(u)),
      ).toBe(false);
    } finally {
      globalThis.confirm = confirm;
    }
    expect(opened).toEqual([]);
  });

  it("opens on an accepted confirmation", () => {
    const opened: string[] = [];
    const confirm = globalThis.confirm;
    globalThis.confirm = () => true;
    try {
      expect(openUrlSafely("slack://channel", (u) => opened.push(u))).toBe(
        true,
      );
    } finally {
      globalThis.confirm = confirm;
    }
    expect(opened).toEqual(["slack://channel"]);
  });
});
