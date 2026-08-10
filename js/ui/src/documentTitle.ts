/** Maximum terminal/surface name length inside the browser document title. */
export const DOCUMENT_ENTITY_TITLE_MAX_LENGTH = 120;

/**
 * Bound an untrusted terminal/surface title without splitting a Unicode code
 * point. The ellipsis counts toward the limit.
 */
export function truncateDocumentEntityTitle(title: string): string {
  let count = 0;
  let ellipsisCutoff = 0;
  for (const char of title) {
    count += 1;
    if (count < DOCUMENT_ENTITY_TITLE_MAX_LENGTH) {
      ellipsisCutoff += char.length;
    } else if (count > DOCUMENT_ENTITY_TITLE_MAX_LENGTH) {
      return `${title.slice(0, ellipsisCutoff)}\u2026`;
    }
  }
  return title;
}
