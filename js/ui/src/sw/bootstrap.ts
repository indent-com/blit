/** The document served to an iframe whose target is unknown, which happens only when a binding was lost. */

export function bootstrapDocument(): Response {
  // `data-blit-preview-lost` is the signal the pane watches for: it re-points
  // the frame at its target rather than leaving a dead preview on screen.
  const html =
    '<!doctype html><meta charset="utf-8"><title>blit preview</title>' +
    '<body data-blit-preview-lost="1" ' +
    'style="margin:0;padding:1rem;font:13px/1.5 ui-monospace,monospace;' +
    'color:#888;background:#1a1a1a">blit preview: reconnecting\u2026</body>';
  return new Response(html, {
    status: 200,
    headers: {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
    },
  });
}
