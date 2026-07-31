/**
 * Shared hunk-line renderer for the diff and commit views. Merges syntax
 * foreground (`colors`, per-char from {@link lineColors}) with the intraline
 * change background (`spans`, byte offsets), emitting runs of same
 * (color, changed) so a token that's only partly changed still colours right.
 */

import type { JSX } from "solid-js";

const dec = new TextDecoder();

export function renderHunkText(
  bytes: Uint8Array,
  spans: Array<[number, number]>,
  changedBg: string,
  colors: (string | null)[],
): JSX.Element {
  const text = dec.decode(bytes);
  const n = text.length;
  if (n === 0) return <>{text}</>;
  // Convert byte-offset change spans to a per-char changed flag.
  const changed = new Array<boolean>(n).fill(false);
  for (const [byteStart, byteLen] of spans) {
    const cs = dec.decode(bytes.subarray(0, byteStart)).length;
    const ce = dec.decode(bytes.subarray(0, byteStart + byteLen)).length;
    for (let i = cs; i < ce && i < n; i++) changed[i] = true;
  }
  const parts: JSX.Element[] = [];
  let i = 0;
  while (i < n) {
    const color = colors[i] ?? null;
    const chg = changed[i];
    let j = i + 1;
    while (j < n && (colors[j] ?? null) === color && changed[j] === chg) j++;
    const seg = text.slice(i, j);
    if (!color && !chg) {
      parts.push(<>{seg}</>);
    } else {
      parts.push(
        <span
          style={{
            color: color ?? undefined,
            background: chg ? changedBg : undefined,
            // Square corners. A change span is a region of the file, and the
            // rows of one edit have to read as one block — a 2px radius drew
            // a notch between every pair of vertically adjacent spans. The
            // inline background already covers the row's full height, so
            // nothing else is needed to make them meet.
          }}
        >
          {seg}
        </span>,
      );
    }
    i = j;
  }
  return <>{parts}</>;
}
