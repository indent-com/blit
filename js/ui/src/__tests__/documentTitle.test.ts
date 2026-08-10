import { describe, expect, it } from "vitest";
import {
  DOCUMENT_ENTITY_TITLE_MAX_LENGTH,
  truncateDocumentEntityTitle,
} from "../documentTitle";

describe("truncateDocumentEntityTitle", () => {
  it("preserves titles at the limit", () => {
    const title = "x".repeat(DOCUMENT_ENTITY_TITLE_MAX_LENGTH);
    expect(truncateDocumentEntityTitle(title)).toBe(title);
  });

  it("truncates oversized titles and includes the ellipsis in the limit", () => {
    const title = "x".repeat(DOCUMENT_ENTITY_TITLE_MAX_LENGTH + 1);
    expect(truncateDocumentEntityTitle(title)).toBe(
      `${"x".repeat(DOCUMENT_ENTITY_TITLE_MAX_LENGTH - 1)}\u2026`,
    );
  });

  it("does not split Unicode code points", () => {
    const prefix = "x".repeat(DOCUMENT_ENTITY_TITLE_MAX_LENGTH - 2);
    expect(truncateDocumentEntityTitle(`${prefix}\ud83d\ude80yz`)).toBe(
      `${prefix}\ud83d\ude80\u2026`,
    );
  });
});
