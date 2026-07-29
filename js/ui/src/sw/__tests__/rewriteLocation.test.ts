import { describe, expect, it } from "vitest";
import type { PreviewTarget } from "@blit-sh/core";
import { rewriteHeaders, rewriteLocation } from "../index";

const target: PreviewTarget = {
  dest: "local",
  scheme: "http",
  host: "localhost",
  port: 3000,
};

describe("rewriteLocation", () => {
  // A redirect is the one response that can move the preview frame somewhere
  // the relay did not choose.
  it("resolves a protocol-relative location against the target", () => {
    // `//host` is an authority, not a path. One naming somewhere else is left
    // to the browser, like any other off-target redirect.
    for (const value of ["//evil.com/x", "//evil.com", "///evil.com/x"]) {
      expect(rewriteLocation(value, target), value).toBeNull();
    }
    // One naming the target becomes a path, so the frame stays in the
    // preview — reading it as a clean path sent it out of the relay instead.
    expect(rewriteLocation("//localhost:3000/next?a=1", target)).toBe(
      "/next?a=1",
    );
    expect(rewriteLocation("//localhost:3000", target)).toBe("/");
  });

  it("keeps genuine same-origin paths", () => {
    expect(rewriteLocation("/login", target)).toBe("/login");
    expect(rewriteLocation("/a/b?c=1#d", target)).toBe("/a/b?c=1#d");
    expect(rewriteLocation("/", target)).toBe("/");
  });

  it("rewrites an on-target absolute redirect to a path", () => {
    expect(rewriteLocation("http://localhost:3000/next?a=1", target)).toBe(
      "/next?a=1",
    );
  });

  it("leaves an off-target absolute redirect alone", () => {
    // Following one would silently proxy a third origin through the relay.
    expect(rewriteLocation("http://evil.com/x", target)).toBeNull();
    expect(rewriteLocation("http://localhost:3001/x", target)).toBeNull();
  });

  it("answers null for something that is not a location at all", () => {
    expect(rewriteLocation("not a url", target)).toBeNull();
    expect(rewriteLocation("", target)).toBeNull();
  });

  // Deliberate: a dev server that bounces you to an identity provider should
  // still get you there, so an off-target redirect is delivered unchanged and
  // the frame follows it out of the relay.
  it("delivers an off-target Location unchanged so the frame follows it", () => {
    for (const value of ["//evil.com/x", "http://evil.com/x", "not a url"]) {
      const headers = rewriteHeaders(
        {
          status: 302,
          statusText: "Found",
          headers: new Headers({ location: value }),
          setCookie: [],
          framing: "none",
          contentLength: 0,
          rest: new Uint8Array(0),
        },
        target,
      );
      expect(headers.get("location"), value).toBe(value);
    }
  });

  it("rewrites an on-target Location to a path", () => {
    for (const value of [
      "http://localhost:3000/next",
      "//localhost:3000/next",
    ]) {
      const headers = rewriteHeaders(
        {
          status: 302,
          statusText: "Found",
          headers: new Headers({ location: value }),
          setCookie: [],
          framing: "none",
          contentLength: 0,
          rest: new Uint8Array(0),
        },
        target,
      );
      expect(headers.get("location"), value).toBe("/next");
    }
  });
});
