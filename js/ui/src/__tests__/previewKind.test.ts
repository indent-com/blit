import { describe, expect, it } from "vitest";
import {
  markdownHeadingSlug,
  resolveRelative,
  workspaceLinkTarget,
} from "../ide/previewKind";

describe("markdown preview links", () => {
  it("resolves paths relative to the markdown file", () => {
    expect(resolveRelative("/repo/docs/guide.md", "../README.md#install")).toBe(
      "/repo/README.md",
    );
    expect(resolveRelative("/repo/docs/guide.md", "./api/index.md?raw=1")).toBe(
      "/repo/docs/api/index.md",
    );
    expect(resolveRelative("/repo/docs/guide.md", "/CONTRIBUTING.md")).toBe(
      "/CONTRIBUTING.md",
    );
  });

  it("opens every local file in its supported view", () => {
    expect(
      workspaceLinkTarget("/repo/docs/guide.md", "../README.md#install"),
    ).toEqual({
      path: "/repo/README.md",
      fragment: "install",
      view: "preview",
    });
    expect(
      workspaceLinkTarget("/repo/README.md", "src/main.ts?plain=1"),
    ).toEqual({
      path: "/repo/src/main.ts",
      fragment: null,
      view: "editor",
    });
    expect(workspaceLinkTarget("/repo/README.md", "assets/logo.png")).toEqual({
      path: "/repo/assets/logo.png",
      fragment: null,
      view: "preview",
    });
  });

  it("keeps fragment links in the current document", () => {
    expect(workspaceLinkTarget("/repo/README.md", "#Install%20notes")).toEqual({
      path: "/repo/README.md",
      fragment: "Install notes",
      view: "preview",
    });
  });

  it("decodes URL-escaped filesystem paths", () => {
    expect(
      workspaceLinkTarget("/repo/README.md", "docs/setup%20notes.md"),
    ).toEqual({
      path: "/repo/docs/setup notes.md",
      fragment: null,
      view: "preview",
    });
  });

  it("leaves network URLs inert", () => {
    expect(
      workspaceLinkTarget("/repo/README.md", "https://example.com/guide.md"),
    ).toBeNull();
    expect(
      workspaceLinkTarget("/repo/README.md", "//example.com/guide.md"),
    ).toBeNull();
  });

  it("creates stable heading ids for fragment links", () => {
    expect(markdownHeadingSlug(" Install & usage ")).toBe("install--usage");
    expect(markdownHeadingSlug("Überblick 2026")).toBe("überblick-2026");
  });
});
