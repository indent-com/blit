/**
 * Classification of hyperlink targets that arrive from the terminal.
 *
 * OSC 8 lets an application decouple a link's *text* from its *target*, which
 * is the same primitive that makes `<a href>` phishable: the cell grid can read
 * `https://your-bank.example` while the target is something else entirely. The
 * regex link detection this supplements could not express that — a URL had to
 * literally appear on screen to be clickable — so accepting OSC 8 is what
 * introduces the risk, and it has to be paid for here.
 *
 * Three outcomes, in the spirit of Ghostty's classifier:
 *
 * - `allow`   — a well-known safe scheme with nothing deceptive in it.
 * - `confirm` — plausible but unverifiable: custom schemes, `file:`, embedded
 *               credentials, non-ASCII hosts. The user is shown the real target
 *               and decides.
 * - `deny`    — never openable: script-executing or content-inlining schemes,
 *               and anything containing characters that can hide what the URL
 *               actually says.
 *
 * The invariant this module exists to protect: **no URL is ever rendered to the
 * user as raw text.** Always display `assessment.display`, never
 * `assessment.raw`. A URL that has passed classification can still contain
 * codepoints that reorder or erase neighbouring text when drawn.
 */

export type UrlVerdict = "allow" | "confirm" | "deny";

export type UrlReason =
  | "ok"
  | "empty"
  | "too-long"
  | "hidden-characters"
  | "no-scheme"
  | "dangerous-scheme"
  | "remote-file"
  | "local-file"
  | "custom-scheme"
  | "embedded-credentials"
  | "deceptive-host";

export interface UrlAssessment {
  verdict: UrlVerdict;
  reason: UrlReason;
  /** One-line explanation, suitable for a confirmation dialog. */
  detail: string;
  /**
   * The URL with every non-printable, invisible, or direction-altering
   * codepoint replaced by a visible `<U+XXXX>` escape. Safe to render as text.
   * This is the only form that should ever be shown to a user.
   */
  display: string;
  /** The target exactly as the application sent it. Never render this. */
  raw: string;
}

/**
 * Schemes we open without asking. Deliberately tiny — every addition is a new
 * way for a terminal application to reach outside the browser sandbox.
 */
const ALLOWED_SCHEMES = new Set(["http", "https", "mailto"]);

/**
 * Schemes that can execute script or inline attacker-chosen content into the
 * page's own origin. These are never openable, not even behind a prompt: there
 * is no legitimate reason for a terminal application to emit one, and no
 * wording of a dialog makes clicking it safe.
 */
const DENIED_SCHEMES = new Set([
  "javascript",
  "data",
  "vbscript",
  "blob",
  "about",
  "filesystem",
  "chrome",
  "chrome-extension",
  "moz-extension",
  "view-source",
  "jar",
]);

/**
 * Longest URL we will look at. Matches `MAX_LINK_URI` on the Rust side; a URL
 * beyond this length is far more likely to be an attempt to bury the real
 * target past the edge of any preview than a genuine link.
 */
const MAX_URL_LENGTH = 4096;

/**
 * Codepoints that can make a URL read as something other than what it is.
 *
 * The bidirectional and zero-width ranges are the load-bearing ones: a single
 * U+202E flips the rendering of everything after it, so `evil.example` can be
 * made to display as `elpmaxe.live`, and zero-width joiners let a host be
 * broken up so it no longer matches what the eye groups together.
 */
function isHiddenCodepoint(cp: number): boolean {
  return (
    cp < 0x20 || // C0 controls, including tab / newline / NUL
    cp === 0x7f || // DEL
    (cp >= 0x80 && cp <= 0x9f) || // C1 controls
    cp === 0xad || // soft hyphen
    cp === 0x61c || // Arabic letter mark
    cp === 0x180e || // Mongolian vowel separator
    (cp >= 0x200b && cp <= 0x200f) || // zero-width space..RLM
    (cp >= 0x202a && cp <= 0x202e) || // bidi embedding / override
    (cp >= 0x2060 && cp <= 0x2064) || // word joiner, invisible operators
    (cp >= 0x2066 && cp <= 0x2069) || // bidi isolates
    cp === 0xfeff || // zero-width no-break space (BOM)
    (cp >= 0xfe00 && cp <= 0xfe0f) || // variation selectors
    (cp >= 0xfff9 && cp <= 0xfffb) || // interlinear annotation
    (cp >= 0xe0000 && cp <= 0xe007f) || // tag characters
    (cp >= 0xe0100 && cp <= 0xe01ef) // variation selectors supplement
  );
}

/**
 * A space is not "hidden", but it has no business in a URL and is a common way
 * to push the real target off the end of a single-line preview.
 */
function isDisallowedSpace(cp: number): boolean {
  return (
    cp === 0x20 ||
    cp === 0xa0 || // no-break space
    (cp >= 0x2000 && cp <= 0x200a) || // en/em/thin spaces
    cp === 0x205f ||
    cp === 0x3000 // ideographic space
  );
}

function hex(cp: number): string {
  return cp.toString(16).toUpperCase().padStart(4, "0");
}

/**
 * Render a URL so that what the user reads is what the URL contains. Every
 * codepoint that would otherwise be invisible or reorder its neighbours is
 * replaced by a literal `<U+XXXX>`.
 *
 * Applied unconditionally, including to URLs that pass classification —
 * escaping only the rejected ones would mean the safe-looking preview is the
 * one you cannot trust.
 */
export function escapeUrlForDisplay(url: string): string {
  let out = "";
  for (const ch of url) {
    const cp = ch.codePointAt(0) ?? 0;
    out +=
      isHiddenCodepoint(cp) || isDisallowedSpace(cp) ? `<U+${hex(cp)}>` : ch;
  }
  return out;
}

/** True when the URL contains anything that could disguise its own text. */
function hasHiddenCharacters(url: string): boolean {
  for (const ch of url) {
    const cp = ch.codePointAt(0) ?? 0;
    if (isHiddenCodepoint(cp) || isDisallowedSpace(cp)) return true;
  }
  return false;
}

/**
 * Extract the scheme without trusting `URL`, which is lenient in ways that
 * matter here (it strips leading control characters before parsing, so
 * `"javascript:alert(1)"` parses as the `javascript:` scheme).
 *
 * Per RFC 3986 a scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`, and
 * percent-encoding is not permitted in it — so an obfuscated `%6Aavascript:`
 * fails to match here and is rejected as having no scheme at all.
 */
function schemeOf(url: string): string | null {
  const m = /^([A-Za-z][A-Za-z0-9+.-]*):/.exec(url);
  return m ? m[1].toLowerCase() : null;
}

function assess(
  verdict: UrlVerdict,
  reason: UrlReason,
  detail: string,
  raw: string,
): UrlAssessment {
  return { verdict, reason, detail, display: escapeUrlForDisplay(raw), raw };
}

/**
 * Classify a hyperlink target from the terminal.
 *
 * Pure and synchronous — call it on hover to build a preview and again on click
 * to decide what to do, rather than caching a verdict across the two.
 */
export function assessUrl(rawUrl: string): UrlAssessment {
  const raw = rawUrl ?? "";

  if (raw.length === 0) {
    return assess("deny", "empty", "The link has no target.", raw);
  }
  if (raw.length > MAX_URL_LENGTH) {
    return assess(
      "deny",
      "too-long",
      `The link is ${raw.length} characters long, which is too long to show you in full.`,
      raw.slice(0, MAX_URL_LENGTH),
    );
  }

  // Checked before the scheme: hidden characters are precisely what would let
  // a dangerous scheme slip past the check below.
  if (hasHiddenCharacters(raw)) {
    return assess(
      "deny",
      "hidden-characters",
      "The link contains invisible or text-reordering characters, so what it displays cannot be trusted to match where it goes.",
      raw,
    );
  }

  const scheme = schemeOf(raw);
  if (scheme === null) {
    return assess(
      "deny",
      "no-scheme",
      "The link has no scheme, so there is no way to tell what opening it would do.",
      raw,
    );
  }
  if (DENIED_SCHEMES.has(scheme)) {
    return assess(
      "deny",
      "dangerous-scheme",
      `The "${scheme}:" scheme can run code or inject content into this page.`,
      raw,
    );
  }

  if (scheme === "file") {
    // A remote file: URL is an SMB/UNC path in disguise; on Windows, opening
    // one leaks credentials to the host named in it.
    const remote = /^file:\/\/(?!\/)([^/]+)/i.exec(raw);
    if (remote && remote[1].toLowerCase() !== "localhost") {
      return assess(
        "deny",
        "remote-file",
        `This link points at a file on another machine (${escapeUrlForDisplay(remote[1])}), which can leak your credentials.`,
        raw,
      );
    }
    return assess(
      "confirm",
      "local-file",
      "This link opens a file on this machine, which may be an executable.",
      raw,
    );
  }

  if (!ALLOWED_SCHEMES.has(scheme)) {
    return assess(
      "confirm",
      "custom-scheme",
      `The "${scheme}:" scheme is handled by another application on this machine.`,
      raw,
    );
  }

  // From here the scheme is http/https/mailto. Two classic look-alike tricks
  // remain, both of which survive a correct scheme.
  if (scheme === "http" || scheme === "https") {
    let parsed: URL | null = null;
    try {
      parsed = new URL(raw);
    } catch {
      return assess("deny", "no-scheme", "The link is not a valid URL.", raw);
    }

    // `https://trusted.example@evil.example/` — everything before the `@` is a
    // username, and the site actually visited is the part most people skim past.
    if (parsed.username || parsed.password) {
      return assess(
        "confirm",
        "embedded-credentials",
        `This link carries a username in front of the real destination, which is ${escapeUrlForDisplay(parsed.hostname)}.`,
        raw,
      );
    }

    // Punycode hosts render as their Unicode form in most UIs, where Cyrillic
    // and Latin lookalikes are indistinguishable.
    if (/(^|\.)xn--/i.test(parsed.hostname)) {
      return assess(
        "confirm",
        "deceptive-host",
        `The host "${escapeUrlForDisplay(parsed.hostname)}" uses characters that can look like a different name.`,
        raw,
      );
    }
    // eslint-disable-next-line no-control-regex
    if (/[^\x00-\x7f]/.test(parsed.hostname)) {
      return assess(
        "confirm",
        "deceptive-host",
        `The host "${escapeUrlForDisplay(parsed.hostname)}" contains non-ASCII characters that can look like a different name.`,
        raw,
      );
    }
  }

  return assess("allow", "ok", "", raw);
}

/**
 * Default activation policy: open `allow` outright, prompt on `confirm`, refuse
 * `deny`. Returns whether the link was opened.
 *
 * Embedders that want their own dialog should classify with {@link assessUrl}
 * and handle the verdict themselves rather than dropping the check — the
 * classification, not the dialog, is what makes this safe.
 */
export function openUrlSafely(
  rawUrl: string,
  open: (url: string) => void = (u) => {
    window.open(u, "_blank", "noopener,noreferrer");
  },
): boolean {
  const a = assessUrl(rawUrl);
  if (a.verdict === "deny") {
    globalThis.alert?.(
      `Blit blocked this link.\n\n${a.display}\n\n${a.detail}`,
    );
    return false;
  }
  if (a.verdict === "confirm") {
    const ok = globalThis.confirm?.(
      `${a.detail}\n\nOpen this link?\n\n${a.display}`,
    );
    if (!ok) return false;
  }
  open(a.raw);
  return true;
}
