/**
 * Applying an LSP rename plan (docs/design/lsp.md `LSP_QUERY_RENAME`).
 *
 * The server answers `RENAME` with `EDIT` records and never touches the
 * filesystem, so the client is what makes a rename happen. Each record
 * names the content version it was computed against (`hash`, BLAKE3-128 —
 * the same value the fs family carries as a u128), which is exactly what
 * `writeFile`'s `ifHash` wants: a plan computed against bytes that have
 * since moved is refused rather than applied to the wrong content.
 *
 * The editor applies its own file through CodeMirror (so the rename lands
 * in the undo history); every other file in the plan is opened here as a
 * short-lived single-file sync, rewritten under CAS, and dropped.
 */

import type {
  BlitWorkspace,
  ConnectionId,
  LspQueryRecord,
} from "@blit-sh/core";
import { FS_ENTRY_NO_CONTENT } from "@blit-sh/core";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** One text edit of a rename plan. */
export type LspEdit = Extract<LspQueryRecord, { kind: "edit" }>;

/** The LSP wire hash (16 bytes, little-endian) as the fs family's u128
 *  bigint. All-zero means "content version unknown" — no CAS base. */
export function lspHashToFs(hash: Uint8Array): bigint {
  let v = 0n;
  for (let i = 15; i >= 0; i--) v = (v << 8n) | BigInt(hash[i] ?? 0);
  return v;
}

/** Offsets of every line start in `text` (index 0 = line 0). */
function lineStarts(text: string): number[] {
  const starts = [0];
  for (let i = text.indexOf("\n"); i !== -1; i = text.indexOf("\n", i + 1)) {
    starts.push(i + 1);
  }
  return starts;
}

/** An LSP position (0-based line, UTF-8 byte column) as a char offset. */
function offsetAt(
  text: string,
  starts: number[],
  line0: number,
  byteCol: number,
): number {
  const li = Math.min(Math.max(line0, 0), starts.length - 1);
  const from = starts[li];
  const to = li + 1 < starts.length ? starts[li + 1] - 1 : text.length;
  const lineText = text.slice(from, to);
  const bytes = encoder.encode(lineText);
  const ch = decoder.decode(
    bytes.subarray(0, Math.min(Math.max(byteCol, 0), bytes.length)),
  ).length;
  return from + Math.min(ch, lineText.length);
}

/** A plan edit resolved to char offsets in a specific document. */
export interface ResolvedEdit {
  from: number;
  to: number;
  insert: string;
}

/**
 * Resolve a file's edits to char offsets, sorted last-first. Applying in
 * descending order keeps every not-yet-applied offset valid, which is what
 * lets both the CodeMirror path and the plain-string path below share one
 * ordering rule. LSP guarantees the edits of one file don't overlap.
 */
export function resolveEdits(text: string, edits: LspEdit[]): ResolvedEdit[] {
  const starts = lineStarts(text);
  return edits
    .map((e) => {
      const from = offsetAt(text, starts, e.line, e.col);
      const to = Math.max(from, offsetAt(text, starts, e.endLine, e.endCol));
      return { from, to, insert: e.newText };
    })
    .sort((a, b) => b.from - a.from);
}

/** Apply resolved edits (descending, as {@link resolveEdits} returns them). */
export function applyResolved(text: string, edits: ResolvedEdit[]): string {
  let out = text;
  for (const e of edits)
    out = out.slice(0, e.from) + e.insert + out.slice(e.to);
  return out;
}

/** What happened to one file of a rename plan. */
export interface RenameFileOutcome {
  /** Workspace-relative path, as the plan named it. */
  path: string;
  edits: number;
  /** Set when the file was left untouched. */
  error?: string;
}

/**
 * Rewrite one file of a rename plan through a short-lived single-file sync.
 *
 * The write is CAS-guarded on the bytes the edits were computed against:
 * the plan's own `hash` when the backend supplied one, else the hash this
 * sync just read. Either way a file that moved under us is refused, never
 * silently clobbered.
 */
export async function applyRenameToFile(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  absPath: string,
  relPath: string,
  edits: LspEdit[],
): Promise<RenameFileOutcome> {
  const fail = (error: string): RenameFileOutcome => ({
    path: relPath,
    edits: 0,
    error,
  });
  let handle;
  try {
    handle = await workspace.syncFs(connectionId, absPath, {
      single: true,
      content: true,
      inlineMax: 8 * 1024 * 1024,
    });
  } catch (e) {
    return fail(e instanceof Error ? e.message : String(e));
  }
  try {
    // syncFs resolves when the server accepts the sync, before any entry
    // lands, so wait for the snapshot to be coherent before reading.
    const node = await new Promise<ReturnType<typeof handle.live.get>>(
      (resolve) => {
        const ready = handle.live.get("");
        if (ready) {
          resolve(ready);
          return;
        }
        const timer = setTimeout(() => {
          unsub();
          resolve(handle.live.get(""));
        }, 10_000);
        const unsub = handle.subscribe(() => {
          const n = handle.live.get("");
          if (!n) return;
          clearTimeout(timer);
          unsub();
          resolve(n);
        });
      },
    );
    if (!node) return fail("not found");

    let bytes = node.content;
    if (!bytes && node.entryFlags & FS_ENTRY_NO_CONTENT) {
      bytes = await handle.fetch("");
    }
    const text = decoder.decode(bytes ?? new Uint8Array());

    // The plan's hash names the bytes it was computed against; a mismatch
    // means the file changed since the query and the offsets no longer
    // describe this content.
    const planHash = lspHashToFs(edits[0].hash);
    if (planHash !== 0n && planHash !== node.hash) {
      return fail("changed on disk since the rename was planned");
    }

    const resolved = resolveEdits(text, edits);
    const next = applyResolved(text, resolved);
    if (next === text) return { path: relPath, edits: 0 };
    await handle.writeFile("", encoder.encode(next), {
      ifHash: node.hash,
      deltaBase: node.hash !== 0n ? (bytes ?? undefined) : undefined,
    });
    return { path: relPath, edits: resolved.length };
  } catch (e) {
    return fail(e instanceof Error ? e.message : String(e));
  } finally {
    handle.stop();
  }
}
