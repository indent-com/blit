/**
 * IdeSession — the single owner of IDE state for one workspace root.
 *
 * The old dock opened fs/git/lsp handles per panel and tore them down on
 * every focus change, losing work and re-deriving reactivity by hand. This
 * inverts that: one session per (connection, root) opens fs + git once (lsp
 * on demand), exposes reactive derived state (tree, git status, commit log,
 * diagnostics) and BSP-agnostic actions, and is ref-counted + idle-cached in
 * a registry so switching terminals reuses a warm session instead of
 * rebuilding it. Panels become pure views over a session; they own no
 * handle lifecycle.
 */

import {
  createRoot,
  createSignal,
  createMemo,
  createEffect,
  onCleanup,
  untrack,
  type Accessor,
} from "solid-js";
import type {
  BlitWorkspace,
  ConnectionId,
  SessionId,
  FsSyncHandle,
  FsNode,
  FsGrepOptions,
  FsGrepResult,
  GitRepoHandle,
  GitStateMirror,
  GitOid,
  LspHandle,
} from "@blit-sh/core";
import {
  FS_ENTRY_TYPE_MASK,
  FS_ENTRY_DIR,
  FS_ENTRY_SYMLINK,
  FS_ENTRY_LINK_DIR,
  gitCommitRecords,
  gitOidHex,
  GIT_LOG_TOPO,
  GIT_COMMITS_MORE,
  GIT_STATUS_OK,
  GIT_CLOSED_CLIENT_REQUEST,
  GIT_CLOSED_CONNECTION_LOST,
  GIT_CLOSED_PERMISSION_LOST,
  GIT_CLOSED_REPO_GONE,
  GIT_CLOSED_RESOURCE_LIMIT,
} from "@blit-sh/core";
import {
  editorAssignment,
  diffAssignment,
  type DiffSide,
} from "@blit-sh/core/bsp";
import { createBlitWorkspaceState } from "@blit-sh/solid";
import { isConnReady, connGeneration, isTransientConnError } from "./reactive";
import { currentSessionForPty } from "./followTerminal";

/** Ceiling on consecutive transient open-retries — see the per-session
 *  counters. Generous: a real reconnect needs one. */
const MAX_OPEN_RETRIES = 20;

/** The per-connection sync cap refused us (docs/design/fs-watch.md
 *  budgets). Transient in practice — slots free as idle warm sessions
 *  expire and dock cards close — so openers re-attempt on a timer. */
export function isSyncLimitError(e: unknown): boolean {
  const msg = e instanceof Error ? e.message : String(e);
  return /resource limit/i.test(msg);
}

/** A `GIT_CLOSED` reason, for the panels that have to explain it. */
function gitClosedText(reason: number): string {
  switch (reason) {
    case GIT_CLOSED_CLIENT_REQUEST:
      return "closed by this client";
    case GIT_CLOSED_REPO_GONE:
      return "the repository went away";
    case GIT_CLOSED_PERMISSION_LOST:
      return "permission lost";
    case GIT_CLOSED_RESOURCE_LIMIT:
      return "the server ran out of file watches";
    default:
      return "the server's repository watch failed";
  }
}

// Shared tree-expansion state: two sessions that resolve to the SAME file tree
// (same connection + resolved root) share one expanded-directory set, so
// switching between panes over a single repo preserves the tree's expansion.
// Created under a persistent root so the signal outlives any one session.
// Bounded: entries past the cap are evicted least-recently-used and their
// roots disposed, so roaming across many (connection, root) trees can't
// accumulate signals forever.
type ExpansionEntry = {
  signal: ReturnType<typeof createSignal<Set<string>>>;
  dispose: () => void;
  lastUsed: number;
};
const sharedExpansion = new Map<string, ExpansionEntry>();
const EXPANSION_CACHE_MAX = 64;
function expansionSignal(
  key: string,
): ReturnType<typeof createSignal<Set<string>>> {
  let entry = sharedExpansion.get(key);
  if (!entry) {
    let signal!: ReturnType<typeof createSignal<Set<string>>>;
    const dispose = createRoot((d) => {
      signal = createSignal<Set<string>>(new Set());
      return d;
    });
    entry = { signal, dispose, lastUsed: 0 };
    sharedExpansion.set(key, entry);
    while (sharedExpansion.size > EXPANSION_CACHE_MAX) {
      let oldestKey: string | null = null;
      let oldest = Infinity;
      for (const [k, e] of sharedExpansion) {
        if (k !== key && e.lastUsed < oldest) {
          oldest = e.lastUsed;
          oldestKey = k;
        }
      }
      if (oldestKey === null) break;
      sharedExpansion.get(oldestKey)!.dispose();
      sharedExpansion.delete(oldestKey);
    }
  }
  entry.lastUsed = Date.now();
  return entry.signal;
}

/** What to open and how to key it. `fromSessionId` (follow-terminal) resolves
 *  the cwd server-side; otherwise `path` is an absolute root. */
export interface IdeSessionDescriptor {
  key: string;
  connectionId: ConnectionId;
  path: string;
  fromSessionId?: SessionId;
  /** The pty behind `fromSessionId`. Follow-terminal sessions are keyed by pty
   *  and stay warm across reconnects, which mint new SessionIds and prune the
   *  superseded ones — so the pty, not the id, is what the opens follow (see
   *  ./followTerminal). */
  fromPtyId?: number;
  /** Tile-anchored roots: once git discovers the enclosing repo, re-root the fs
   *  tree at the repo workdir so the explorer shows the whole project rather
   *  than the file's directory. Falls back to `path` when not in a repo. */
  preferRepoRoot?: boolean;
}

export interface IdeTreeRow {
  relPath: string;
  name: string;
  depth: number;
  type: number;
  flags: number;
  size: number;
  expanded: boolean;
}

export interface IdeCommitRow {
  /** Full hex oid (for DAG parent matching). */
  oid: string;
  /** Abbreviated oid for display. */
  short: string;
  /** Full hex parent oids, first-parent first. */
  parents: string[];
  subject: string;
  author: string;
  /** Author time in seconds — displayed with the author name (git log's
   *  convention). Walk ORDER stays committer-date, server-side. */
  time: bigint;
}

export interface IdeSession {
  readonly key: string;
  readonly connectionId: ConnectionId;
  /** Canonical root path on the server, once the fs sync opens. */
  root: Accessor<string | null>;
  fsError: Accessor<string | null>;

  // ── Explorer ─────────────────────────────────────────────────────────
  /** Flattened visible tree rows; `null` until the root sync opens. */
  tree: Accessor<IdeTreeRow[] | null>;
  /** "opening" → no handle yet; "loading" → snapshot streaming; "live". */
  treePhase: Accessor<"opening" | "loading" | "live">;
  toggleDir(relPath: string): void;
  isExpanded(relPath: string): boolean;
  /** Expand a directory and all its ancestors (e.g. to follow a terminal cwd). */
  expandTo(relDir: string): void;

  // ── SCM / branch ─────────────────────────────────────────────────────
  /** Live git state (status/head/upstreams/stashes); `null` until watched. */
  gitState: Accessor<GitStateMirror | null>;
  gitHandle: Accessor<GitRepoHandle | null>;
  /** Why this root has no repo: the open failed (not a repository, the source
   *  terminal is gone, …) or the server closed the watch. `null` while the
   *  repo is opening or open — the two states panels must not conflate. */
  gitError: Accessor<string | null>;
  /** There is no repo here and none is coming: the open failed, the server
   *  closed the watch, or the remote has no git support. False while one is
   *  still opening (or could be, once the transport is back), so panels that
   *  only make sense over a repository can fold away without flapping. */
  noRepo: Accessor<boolean>;
  /** The repo's worktree root, discovered when git first opens and kept across
   *  connection resets (unlike gitHandle, which drops to null on a reset). Use
   *  this to build commit tiles so clicking a commit still works while git is
   *  re-attaching. */
  repoWorkdir: Accessor<string | null>;

  // ── Commit log ───────────────────────────────────────────────────────
  commits: Accessor<IdeCommitRow[]>;
  /** More history is available past the loaded pages. */
  hasMoreLog: Accessor<boolean>;
  /** Fetch and append the next page of older commits (frontier pagination). */
  loadMoreLog(): void;
  /** The revision spec the log walks — whitespace-separated expressions,
   *  merged like `git rev-list` args (`base..a b ^c`); empty = HEAD
   *  (docs/design/git.md `GIT_RESOLVE`). */
  logSpec: Accessor<string>;
  /** Set the spec and persist it per repo workdir in the server KV store
   *  (`log/spec/<workdir>`), so a reload restores the range being studied.
   *  Empty (or plain HEAD) deletes the stored spec. */
  setLogSpec(spec: string): void;
  /** The last spec's resolution failure, cleared on success. */
  logSpecError: Accessor<string | null>;
  /** A page for the current spec has arrived (false while loading). */
  logLoaded: Accessor<boolean>;

  // ── Diagnostics (lsp, lazy) ──────────────────────────────────────────
  /** Idempotently attach a language server for this root. */
  ensureLsp(): void;
  lspHandle: Accessor<LspHandle | null>;
  /** The remote has no language intelligence, so nothing will ever attach —
   *  from the negotiated features, not from a failed attach, since the attach
   *  is lazy. False while the transport is down. */
  noLsp: Accessor<boolean>;
  /** Bumps on every lsp state/diagnostics push. */
  lspVersion: Accessor<number>;

  // ── Actions → BSP tile assignments (the caller places them) ──────────
  fileAssignment(relPath: string): string;
  diffAssignment(relPath: string, side?: DiffSide): string;

  // ── File operations (docs/design/fs-write.md `FS_OP`): root-relative
  //    paths; rename doubles as move; remove is recursive for dirs. The
  //    watcher streams the result back into the tree — no manual refresh.
  createFile(relPath: string): Promise<void>;
  createDir(relPath: string): Promise<void>;
  renamePath(from: string, to: string): Promise<void>;
  removePath(relPath: string): Promise<void>;

  /** Content search under this session's root (docs/design/fs-grep.md).
   *  Hits are grouped by file, tracked files first and gitignored ones
   *  last — ignore rules rank here, they do not exclude. Rejects with the
   *  server's own wording, so an uncompilable regex explains itself. */
  grep(query: string, opts?: FsGrepOptions): Promise<FsGrepResult>;
}

function typeOf(node: FsNode): number {
  return node.entryFlags & FS_ENTRY_TYPE_MASK;
}

/** A row the tree can descend: a real directory, or a symlink whose target is
 *  one. The sync enumerates both (crates/fssync `NodeMeta::enumerable_dir`), so
 *  gating expansion on `FS_ENTRY_DIR` alone renders a symlinked directory as a
 *  leaf whose children exist but can never be reached. */
export function isDirLike(type: number, flags: number): boolean {
  return (
    type === FS_ENTRY_DIR ||
    (type === FS_ENTRY_SYMLINK && (flags & FS_ENTRY_LINK_DIR) !== 0)
  );
}

/** Build one IdeSession. Runs inside its own reactive root (see the registry
 *  below) so handle subscriptions and cleanups are scoped to the session. */
function buildSession(
  workspace: BlitWorkspace,
  desc: IdeSessionDescriptor,
): IdeSession {
  const { connectionId } = desc;

  // Gate every open (fs/git/lsp) on the connection being ready, and re-open
  // after a reset. A session can be built before its transport is connected —
  // a restored tile-anchored root on reload, or a declared root on a
  // still-connecting remote — which would otherwise throw "Cannot sync while
  // transport is connecting" as a permanent error. The generation bump also
  // recovers these handles after a server re-establish (they don't survive a
  // reset even when the transport stays "connected").
  const wsSnap = createBlitWorkspaceState(workspace);
  const fsReady = createMemo(() =>
    isConnReady(wsSnap(), connectionId, "supportsFsSync"),
  );
  const gitReady = createMemo(() =>
    isConnReady(wsSnap(), connectionId, "supportsGit"),
  );
  const lspReady = createMemo(() =>
    isConnReady(wsSnap(), connectionId, "supportsLsp"),
  );
  const connGen = createMemo(() => connGeneration(wsSnap(), connectionId));

  // The source of a follow-terminal open (the server resolves the root from
  // that pty's live cwd), resolved fresh at every open: SessionIds are minted
  // per connection generation, and this session outlives generations — it is
  // keyed by pty and kept warm across reconnects (see ./followTerminal).
  // Untracked: the session list churns on every title/row update, and reading
  // it reactively would tear the handles down and re-open them for nothing.
  const fromSessionId = (): SessionId | undefined => {
    const id = desc.fromSessionId;
    if (!id || desc.fromPtyId === undefined) return id;
    return currentSessionForPty(
      untrack(wsSnap).sessions,
      connectionId,
      desc.fromPtyId,
      id,
    );
  };

  // Retry counters. A re-establish resets fs/git/lsp *after* re-emitting the
  // snapshot (BlitConnection S2C_HELLO: emit() then resetFsSyncs/resetGitRepos),
  // and an open registers its pending entry synchronously — so the open issued
  // during that emit is immediately rejected with a transient "Connection
  // re-established". Bumping the retry counter in that transient .catch re-runs
  // the effect a microtask later, after the reset, so the re-attempt sticks.
  const [fsRetry, setFsRetry] = createSignal(0);
  const [gitRetry, setGitRetry] = createSignal(0);
  const [lspRetry, setLspRetry] = createSignal(0);
  // A real reset needs one retry; this ceiling is pure defense-in-depth so that
  // if the "ready ⟹ transport connected" invariant were ever broken, a
  // synchronously-rejecting open can't self-reschedule into a tab-freezing
  // microtask loop. Reset to 0 on every successful open, so a long-lived
  // session with many genuine reconnects never exhausts it.
  let fsRetries = 0;
  let gitRetries = 0;
  let lspRetries = 0;

  // The repo workdir, discovered when git opens — lets a tile-anchored session
  // re-root its fs tree at the whole repo instead of the file's directory.
  const [gitWorkdir, setGitWorkdir] = createSignal<string | null>(null);
  const [gitSettled, setGitSettled] = createSignal(false);

  // Where to root the fs tree. Normally the descriptor path; for a tile-anchored
  // (preferRepoRoot) session, the discovered repo workdir. Returns null while
  // git is still resolving, so the tree opens once — at the repo — rather than
  // flashing the file's directory first, then falls back to the file's
  // directory when git settles without a repo (or the remote has no git).
  const effectiveRootPath = (): string | null => {
    if (!desc.preferRepoRoot) return desc.path;
    const wd = gitWorkdir();
    if (wd) return wd;
    if (gitSettled()) return desc.path; // git resolved, not a repo → file dir
    if (fsReady() && !gitReady()) return desc.path; // connected, no git support
    return null; // git is (or will be) available; wait for the workdir
  };

  // ── fs: a lazy per-directory tree (non-recursive root + per-expanded-dir
  // syncs). Metadata only; `.git` filtered out client-side. ──────────────
  const [rootFs, setRootFs] = createSignal<FsSyncHandle | null>(null);
  const [rootPath, setRootPath] = createSignal<string | null>(null);
  const [fsError, setFsError] = createSignal<string | null>(null);
  const [phase, setPhase] = createSignal<"opening" | "loading" | "live">(
    "opening",
  );
  // Expansion is shared per (connection, resolved root). Before the root path
  // is known (the tree isn't shown yet) fall back to a local set; once rootPath
  // resolves, reads/writes hit the shared signal, so same-root panes sync.
  const [localExpanded, setLocalExpanded] = createSignal<Set<string>>(
    new Set(),
  );
  const expansionKey = (): string | null => {
    const r = rootPath();
    return r ? `${connectionId}\u0000${r}` : null;
  };
  const expanded = (): Set<string> => {
    const k = expansionKey();
    return k ? expansionSignal(k)[0]() : localExpanded();
  };
  const setExpanded = (updater: (prev: Set<string>) => Set<string>): void => {
    const k = expansionKey();
    if (k) expansionSignal(k)[1](updater);
    else setLocalExpanded(updater);
  };
  const [fsVersion, setFsVersion] = createSignal(0);
  // One non-recursive sync per expanded directory (rel path → handle).
  const childHandles = new Map<string, FsSyncHandle>();
  const [childVersion, setChildVersion] = createSignal(0);

  const bumpFs = () => setFsVersion((v) => v + 1);

  // The ROOT sync drives the tree phase (opening/loading/live). Child syncs
  // must not — a child directory's RESET/SYNC would otherwise flip the whole
  // explorer's phase.
  const rootOpts = {
    recursive: false,
    content: false,
    onSync: () => setPhase("live"),
    onUpdate: bumpFs,
    onReset: () => setPhase("loading"),
  };
  const childOpts = { recursive: false, content: false, onUpdate: bumpFs };

  // Once the session's reactive root is disposed, a late-resolving open must
  // tear its handle down instead of storing it into a dead session.
  let disposed = false;
  // Directories whose child sync is in flight, so concurrent opens coalesce.
  const pending = new Set<string>();

  function stopChildren() {
    for (const h of childHandles.values()) h.stop();
    childHandles.clear();
  }

  // Root sync — gated on fs readiness, re-opened on reset. Its onCleanup tears
  // the run's root + child syncs down before the next open.
  createEffect(() => {
    connGen(); // re-open after a connection reset
    fsRetry(); // re-attempt after a transient (reset-clobbered) open
    const rootAt = effectiveRootPath();
    if (!fsReady() || rootAt === null) {
      // Not connected yet, or waiting on git to reveal the repo root — wait;
      // this effect re-runs when it becomes ready.
      setFsError(null);
      setPhase("opening");
      return;
    }
    let localDisposed = false;
    let limitTimer: ReturnType<typeof setTimeout> | null = null;
    workspace
      .syncFs(connectionId, rootAt, {
        ...rootOpts,
        fromSessionId: fromSessionId(),
      })
      .then((h) => {
        if (disposed || localDisposed) {
          h.stop();
          return;
        }
        fsRetries = 0;
        setFsError(null);
        setRootFs(h);
        setRootPath(h.root);
      })
      .catch((e: unknown) => {
        if (disposed || localDisposed) return;
        // Transient (the open raced a re-establish reset) — retry after the
        // reset settles. Otherwise surface the real error.
        if (isTransientConnError(e) && fsRetries++ < MAX_OPEN_RETRIES) {
          setFsError(null);
          setPhase("opening");
          setFsRetry((n) => n + 1);
        } else if (isSyncLimitError(e)) {
          // The per-connection sync cap is transient in practice: slots
          // free as idle warm sessions expire and dock cards close. Show
          // the error but keep re-attempting instead of bricking the tree.
          setFsError(e instanceof Error ? e.message : String(e));
          limitTimer = setTimeout(() => setFsRetry((n) => n + 1), 3000);
        } else {
          setFsError(e instanceof Error ? e.message : String(e));
        }
      });
    onCleanup(() => {
      localDisposed = true;
      if (limitTimer) clearTimeout(limitTimer);
      rootFs()?.stop();
      stopChildren();
      setRootFs(null);
    });
  });

  onCleanup(() => {
    disposed = true;
    // Belt: the root effect's per-run cleanup already stops the tree, but a
    // child sync could have been stored in a window where that cleanup won't
    // run again — guarantee teardown on disposal.
    rootFs()?.stop();
    stopChildren();
  });

  function openChild(relDir: string) {
    if (childHandles.has(relDir) || pending.has(relDir)) return;
    const r = rootFs();
    if (!r) return;
    pending.add(relDir);
    workspace
      .syncFs(connectionId, `${r.root}/${relDir}`, childOpts)
      .then((h) => {
        pending.delete(relDir);
        // Dropped while opening (collapsed, removed, disposed, or already
        // opened by a racing reconcile): discard this handle.
        if (disposed || !expanded().has(relDir) || childHandles.has(relDir)) {
          h.stop();
          return;
        }
        childHandles.set(relDir, h);
        setChildVersion((v) => v + 1);
      })
      .catch(() => {
        pending.delete(relDir);
      });
  }

  // toggleDir only flips the expanded set; the reconcile effect below opens
  // and stops the child syncs to match the visible expanded tree.
  function toggleDir(relDir: string) {
    setExpanded((cur) => {
      const next = new Set(cur);
      if (next.has(relDir)) next.delete(relDir);
      else next.add(relDir);
      return next;
    });
  }

  /** Expand `relDir` and every ancestor so it becomes visible in the tree.
   *  No-op if already fully expanded (keeps the signal reference stable). */
  function expandTo(relDir: string) {
    if (!relDir) return;
    const parts = relDir.split("/").filter(Boolean);
    setExpanded((cur) => {
      let changed = false;
      const next = new Set(cur);
      let acc = "";
      for (const p of parts) {
        acc = acc ? `${acc}/${p}` : p;
        if (!next.has(acc)) {
          next.add(acc);
          changed = true;
        }
      }
      return changed ? next : cur;
    });
  }

  type ChildEntry = Omit<IdeTreeRow, "depth" | "relPath" | "expanded">;
  const NO_CHILDREN: ChildEntry[] = [];
  // Per-directory sorted child lists, reused while that directory's own sync
  // saw no update (each handle's revision bumps only for its own dir), so an
  // fs event in one directory doesn't re-sort every other visible directory.
  const childListCache = new WeakMap<
    FsSyncHandle,
    { rev: number; children: ChildEntry[] }
  >();

  function childrenOf(handle: FsSyncHandle | undefined): ChildEntry[] {
    if (!handle) return NO_CHILDREN;
    const cached = childListCache.get(handle);
    if (cached && cached.rev === handle.revision) return cached.children;
    const out: ChildEntry[] = [];
    for (const [name, node] of handle.live) {
      if (name === "" || name === ".git") continue;
      out.push({
        name,
        type: typeOf(node),
        flags: node.entryFlags,
        size: node.size,
      });
    }
    out.sort((a, b) => {
      const ad = isDirLike(a.type, a.flags) ? 0 : 1;
      const bd = isDirLike(b.type, b.flags) ? 0 : 1;
      return ad !== bd ? ad - bd : a.name.localeCompare(b.name);
    });
    childListCache.set(handle, { rev: handle.revision, children: out });
    return out;
  }

  // Row objects are cached by relPath and reused while their fields are
  // unchanged, so `<For>` keeps the DOM of untouched rows across fs events;
  // the memo's equals keeps the array identity too when nothing changed.
  let rowCache = new Map<string, IdeTreeRow>();
  const tree = createMemo<IdeTreeRow[] | null>(
    () => {
      fsVersion();
      childVersion();
      const exp = expanded();
      const r = rootFs();
      if (!r) return null;
      const out: IdeTreeRow[] = [];
      const nextCache = new Map<string, IdeTreeRow>();
      const walk = (
        relDir: string,
        handle: FsSyncHandle | undefined,
        depth: number,
      ) => {
        for (const e of childrenOf(handle)) {
          const relPath = relDir ? `${relDir}/${e.name}` : e.name;
          const isExpanded = exp.has(relPath);
          const prev = rowCache.get(relPath);
          const row =
            prev &&
            prev.depth === depth &&
            prev.type === e.type &&
            prev.flags === e.flags &&
            prev.size === e.size &&
            prev.expanded === isExpanded
              ? prev
              : {
                  relPath,
                  name: e.name,
                  depth,
                  type: e.type,
                  flags: e.flags,
                  size: e.size,
                  expanded: isExpanded,
                };
          nextCache.set(relPath, row);
          out.push(row);
          if (isDirLike(e.type, e.flags) && isExpanded) {
            walk(relPath, childHandles.get(relPath), depth + 1);
          }
        }
      };
      walk("", r, 0);
      rowCache = nextCache;
      return out;
    },
    null,
    {
      equals: (a, b) =>
        a === b ||
        (a !== null &&
          b !== null &&
          a.length === b.length &&
          a.every((row, i) => row === b[i])),
    },
  );

  // Reconcile live child syncs against the visible, expanded tree: stop any
  // sync whose directory is no longer an expanded row (collapsed, a collapsed
  // ancestor, or removed on disk) and open one for every expanded dir that
  // lacks it (revealed by expanding a parent, or recreated on disk). This is
  // what actually opens/closes child syncs — toggleDir just flips `expanded`.
  createEffect(() => {
    const rows = tree();
    if (!rows) return;
    const wanted = new Set<string>();
    for (const row of rows) {
      if (isDirLike(row.type, row.flags) && row.expanded)
        wanted.add(row.relPath);
    }
    for (const relDir of [...childHandles.keys()]) {
      if (!wanted.has(relDir)) {
        childHandles.get(relDir)!.stop();
        childHandles.delete(relDir);
      }
    }
    for (const relDir of wanted) openChild(relDir);
  });

  // ── git: watch status + branch + a head commit-log page ────────────────
  const [gitHandle, setGitHandle] = createSignal<GitRepoHandle | null>(null);
  const [gitVersion, setGitVersion] = createSignal(0);
  // Why there is no repo, when that is a failure rather than "still opening".
  // Without it every git panel has to guess, and the commit log's guess was
  // "loading" — forever, for a page no one is coming to send.
  const [gitError, setGitError] = createSignal<string | null>(null);
  // Settled emptiness: an open that failed for good, or a remote whose server
  // has no git at all (the open effect never runs, so there is no error to
  // read either). Deliberately false while the transport is down — the repo
  // that comes back is not news, and folding panels on a blip is.
  const noRepo = createMemo(
    () => !gitHandle() && (gitError() !== null || (fsReady() && !gitReady())),
  );
  // Commit log with frontier pagination: a live head page (watchLog, updates
  // as refs move) plus statically fetched older pages appended on demand.
  const LOG_PAGE = 1000;
  const [headRows, setHeadRows] = createSignal<IdeCommitRow[]>([]);
  const [tailRows, setTailRows] = createSignal<IdeCommitRow[]>([]);
  const [logFrontier, setLogFrontier] = createSignal<GitOid[]>([]);
  const [hasMoreLog, setHasMoreLog] = createSignal(false);
  // The revision spec the log walks (empty = HEAD), its resolved hides
  // (pagination must re-issue the SAME hides with the frontier tips,
  // docs/design/git.md `GIT_LOG`), and the last resolution failure.
  const [logSpec, setLogSpec] = createSignal("");
  const [logHides, setLogHides] = createSignal<GitOid[]>([]);
  const [logSpecError, setLogSpecError] = createSignal<string | null>(null);
  // A page for the current spec has arrived — before that the log is
  // loading, which is not the same as a genuinely empty log.
  const [logLoaded, setLogLoaded] = createSignal(false);
  let watchedSpec: string | null = null;
  let headTop: string | null = null;
  let loadingMore = false;

  const buildCommitRows = (
    records: Uint8Array,
    h: GitRepoHandle,
  ): IdeCommitRow[] => {
    const rows: IdeCommitRow[] = [];
    for (const rec of gitCommitRecords(records)) {
      if (rec.kind !== "commit") continue;
      const hex = gitOidHex(rec.oid, h.oidFormat);
      rows.push({
        oid: hex,
        short: hex.slice(0, 8),
        parents: rec.parents.map((p) => gitOidHex(p, h.oidFormat)),
        subject: rec.message.split("\n")[0],
        author: rec.authorName,
        time: rec.authorTime,
      });
    }
    return rows;
  };

  // A re-pushed head page mostly repeats the previous one (any ref settle
  // re-streams it). Reuse unchanged row objects — and the previous array
  // outright when nothing moved — so downstream memos (commits, graph
  // layout) and `<For>` DOM see stable identities.
  const sameCommitRow = (a: IdeCommitRow, b: IdeCommitRow): boolean =>
    a.oid === b.oid &&
    a.subject === b.subject &&
    a.author === b.author &&
    a.time === b.time &&
    a.parents.length === b.parents.length &&
    a.parents.every((p, i) => p === b.parents[i]);
  const reuseCommitRows = (
    prev: IdeCommitRow[],
    next: IdeCommitRow[],
  ): IdeCommitRow[] => {
    const byOid = new Map<string, IdeCommitRow>();
    for (const r of prev) byOid.set(r.oid, r);
    let allSame = prev.length === next.length;
    const out = next.map((r, i) => {
      const p = byOid.get(r.oid);
      if (p && sameCommitRow(p, r)) {
        if (p !== prev[i]) allSame = false;
        return p;
      }
      allSame = false;
      return r;
    });
    return allSame ? prev : out;
  };

  // Head (live) then tail (paginated), deduped by oid — the frontier's first
  // commit can repeat the head's boundary.
  const commits = createMemo<IdeCommitRow[]>(() => {
    const seen = new Set<string>();
    const out: IdeCommitRow[] = [];
    for (const c of headRows()) {
      seen.add(c.oid);
      out.push(c);
    }
    for (const c of tailRows()) {
      if (!seen.has(c.oid)) {
        seen.add(c.oid);
        out.push(c);
      }
    }
    return out;
  });

  async function loadMoreLog(): Promise<void> {
    const h = gitHandle();
    if (!h || loadingMore || !hasMoreLog()) return;
    const tips = logFrontier();
    if (tips.length === 0) {
      setHasMoreLog(false);
      return;
    }
    loadingMore = true;
    try {
      const page = await h.log({
        flags: GIT_LOG_TOPO,
        limit: LOG_PAGE,
        tips,
        hides: logHides(),
      });
      setTailRows((prev) => [...prev, ...buildCommitRows(page.records, h)]);
      setLogFrontier(page.frontier);
      setHasMoreLog(page.frontier.length > 0);
    } catch {
      setHasMoreLog(false);
    } finally {
      loadingMore = false;
    }
  }

  // Repo open — gated on git readiness, re-opened on reset. The log
  // subscription is held in the effect and closed by its onCleanup on re-open
  // or disposal.
  createEffect(() => {
    connGen(); // re-open after a connection reset
    gitRetry(); // re-attempt after a transient (reset-clobbered) open
    if (!gitReady()) return; // wait; a reset closes the old handle below
    let localDisposed = false;
    workspace
      .openRepo(connectionId, desc.path, {
        watch: true,
        status: true,
        // Include untracked files: a freshly-created file must show up under
        // Changes. This costs a worktree walk on first status, but the live
        // watch keeps subsequent updates incremental.
        untracked: true,
        tracking: true,
        fromSessionId: fromSessionId(),
        onState: () => setGitVersion((v) => v + 1),
        // Reflect a close/connection-loss in the reactive graph too. A
        // server-side close (the watch hit a resource limit, the repo went
        // away) is terminal for this handle: drop it and say why, or every
        // git-backed panel would sit on a dead repo — the commit log showing
        // "Loading…" for a page that can never arrive. A connection loss is
        // not terminal; connGen re-opens.
        onClosed: (reason: number) => {
          setGitVersion((v) => v + 1);
          if (reason === GIT_CLOSED_CONNECTION_LOST || disposed) return;
          setGitHandle(null);
          setGitError(`Repository watch closed: ${gitClosedText(reason)}`);
        },
      })
      .then((h) => {
        if (disposed || localDisposed) {
          h.close();
          return;
        }
        gitRetries = 0;
        setGitError(null);
        setGitHandle(h);
        // Reveal the repo root so a tile-anchored fs tree can re-root at it.
        setGitWorkdir(h.workdir);
        setGitSettled(true);
      })
      .catch((e: unknown) => {
        if (disposed || localDisposed) return;
        // Transient (the open raced a re-establish reset) — retry, so a
        // preferRepoRoot tree still learns the repo workdir instead of
        // mis-rooting at the file's directory. A genuine failure (not a repo),
        // or too many transient retries, settles — letting the fs tree fall
        // back to the descriptor path, and telling the git panels why they
        // have no repo instead of leaving them to look like they are loading.
        if (isTransientConnError(e) && gitRetries++ < MAX_OPEN_RETRIES)
          setGitRetry((n) => n + 1);
        else {
          setGitError(e instanceof Error ? e.message : String(e));
          setGitSettled(true);
        }
      });
    onCleanup(() => {
      localDisposed = true;
      gitHandle()?.close();
      setGitHandle(null);
    });
  });

  // Restore the persisted log spec once per workdir (the key setLogSpec
  // writes below). A spec the user typed before the repo settled wins —
  // and gets stored once the workdir lands, instead of being dropped.
  let logSpecLoadedFor: string | null = null;
  let pendingSpecStore: string | null = null;
  function storeLogSpec(workdir: string, spec: string): void {
    const key = `log/spec/${workdir}`;
    const trimmed = spec.trim();
    if (trimmed && trimmed !== "HEAD")
      workspace
        .kvPut(connectionId, key, new TextEncoder().encode(trimmed))
        .catch(() => {});
    else workspace.kvDelete(connectionId, key).catch(() => {});
  }
  createEffect(() => {
    const workdir = gitWorkdir();
    if (!workdir || logSpecLoadedFor === workdir) return;
    logSpecLoadedFor = workdir;
    if (pendingSpecStore !== null) {
      storeLogSpec(workdir, pendingSpecStore);
      pendingSpecStore = null;
      return;
    }
    workspace
      .kvFetch(connectionId, `log/spec/${workdir}`)
      .then((res) => {
        if (disposed || !res) return;
        const spec = new TextDecoder().decode(res.value).trim();
        if (spec && logSpec() === "") setLogSpec(spec);
      })
      .catch(() => {});
  });
  function setAndStoreLogSpec(spec: string): void {
    setLogSpec(spec);
    const workdir = gitWorkdir();
    if (workdir) storeLogSpec(workdir, spec);
    else pendingSpecStore = spec;
  }

  // Log watch — its own effect so changing the spec re-subscribes without
  // reopening the repo. The subscription streams the head page and re-walks
  // whenever the resolved endpoints move. Cached rows are cleared only on
  // a SPEC change (a different range is a different log) — a handle
  // recycle (re-establish) keeps showing them until the fresh page lands,
  // so the panel never flashes "No commits." over a populated log.
  createEffect(() => {
    const h = gitHandle();
    const spec =
      logSpec().trim() || ["HEAD", opRefTips()].filter(Boolean).join(" ");
    if (spec !== watchedSpec) {
      watchedSpec = spec;
      setHeadRows([]);
      setTailRows([]);
      setLogFrontier([]);
      setHasMoreLog(false);
      setLogLoaded(false);
      setLogSpecError(null);
      setLogHides([]);
    }
    if (!h) return; // keep cached rows while the repo re-attaches
    // A fresh subscription restarts pagination once its first page lands
    // (the old frontier belonged to the previous walk).
    headTop = null;
    // Pagination needs the spec's hides alongside the frontier tips; a
    // plain HEAD log hides nothing.
    if (spec !== "HEAD") {
      h.resolve(spec).then(
        (r) => setLogHides(r.hides),
        () => {},
      );
    }
    const sub = h.watchLog(
      spec,
      // Topological order (not first-parent) so merges keep all parents —
      // the log renders the full commit DAG.
      { flags: GIT_LOG_TOPO, limit: LOG_PAGE },
      (page) => {
        setLogLoaded(true);
        if (page.status !== GIT_STATUS_OK) {
          setLogSpecError(`Cannot resolve "${spec}"`);
          return;
        }
        setLogSpecError(null);
        const rows = buildCommitRows(page.records, h);
        setHeadRows((prev) => reuseCommitRows(prev, rows));
        const top = rows[0]?.oid ?? null;
        if (top !== headTop) {
          // The head moved — restart pagination from the fresh frontier.
          headTop = top;
          setTailRows([]);
          setLogFrontier(page.frontier);
          setHasMoreLog((page.flags & GIT_COMMITS_MORE) !== 0);
        }
      },
    );
    onCleanup(() => sub.close());
  });

  // `equals: false` — the mirror is mutated in place, so each push returns the
  // SAME GitStateMirror reference. Without this, Solid's referential-equality
  // check would suppress the update and the SCM panel would never re-render
  // when the worktree changes.
  const gitState = createMemo<GitStateMirror | null>(
    () => {
      gitVersion();
      return gitHandle()?.state ?? null;
    },
    null,
    { equals: false },
  );

  // While an operation is in progress, the default HEAD walk also merges
  // in the operation's tips (MERGE_HEAD, REBASE_HEAD, ORIG_HEAD, … —
  // streamed as no-refs/-prefix STATE_REF records, docs/design/git.md):
  // the commits being merged or replayed are usually NOT ancestors of
  // HEAD, so without their tips the log can't show their pills. A memo on
  // the joined-oids STRING keeps the watch stable across state pushes —
  // the log-watch effect re-subscribes only when the tips actually change.
  // (Declared after gitState: memos compute eagerly at creation.)
  const opRefTips = createMemo(() => {
    const gs = gitState();
    if (!gs?.op) return "";
    const fmt = gitHandle()?.oidFormat;
    const tips: string[] = [];
    for (const [name, ref] of gs.refs) {
      if (/^[A-Z_]+(#\d+)?$/.test(name)) tips.push(gitOidHex(ref.oid, fmt));
    }
    return tips.sort().join(" ");
  });

  // ── lsp: attached on demand (Problems / editor squiggles) ──────────────
  const [lspHandle, setLspHandle] = createSignal<LspHandle | null>(null);
  const [lspVersion, setLspVersion] = createSignal(0);
  // The remote cannot do language intelligence at all (old blit, or BLIT_LSP=0,
  // which unadvertises the feature). Read off the negotiated features rather
  // than a failed attach: the attach is lazy, so a panel that folds itself away
  // on this would otherwise have to open first to learn it should not have.
  const noLsp = createMemo(() => fsReady() && !lspReady());
  // Requested once a consumer needs it; the gated effect below opens it when
  // the connection is ready and re-opens after a reset (a plain one-shot open
  // would be lost forever if it fired while the transport was still connecting).
  const [lspWanted, setLspWanted] = createSignal(false);
  function ensureLsp() {
    setLspWanted(true);
  }

  createEffect(() => {
    if (!lspWanted()) return;
    connGen(); // re-open after a connection reset
    lspRetry(); // re-attempt after a transient (reset-clobbered) open
    if (!lspReady()) return; // no transport yet, or no language support at all
    let localDisposed = false;
    let unsub: (() => void) | null = null;
    workspace
      .openLsp(connectionId, desc.path, {
        diagnostics: true,
        fromSessionId: fromSessionId(),
      })
      .then((h) => {
        if (disposed || localDisposed) {
          h.close();
          return;
        }
        lspRetries = 0;
        setLspHandle(h);
        unsub = h.subscribe(() => setLspVersion((v) => v + 1));
      })
      .catch((e: unknown) => {
        // Best-effort: retry only a transient race with a re-establish reset;
        // a real failure (no language server) stays silent.
        if (
          !disposed &&
          !localDisposed &&
          isTransientConnError(e) &&
          lspRetries++ < MAX_OPEN_RETRIES
        )
          setLspRetry((n) => n + 1);
      });
    onCleanup(() => {
      localDisposed = true;
      unsub?.();
      lspHandle()?.close();
      setLspHandle(null);
    });
  });

  // `rootPath` is only known from the FS_SYNCED echo, so it is null before the
  // root sync lands and again after a reset clears it — while previously
  // rendered rows stay clickable. Falling back to the bare relPath used to send
  // a *relative* path to the server, which resolves against the server's own
  // cwd rather than the synced tree: on a remote that is some unrelated
  // directory, and the editor reported the file as simply "not found". An empty
  // string is refused by the caller instead, which can say so.
  const abs = (relPath: string): string => {
    const r = rootPath();
    return r ? `${r}/${relPath}` : "";
  };

  return {
    key: desc.key,
    connectionId,
    root: rootPath,
    fsError,
    tree,
    treePhase: phase,
    toggleDir,
    isExpanded: (relPath) => expanded().has(relPath),
    expandTo,
    gitState,
    gitHandle,
    gitError,
    noRepo,
    repoWorkdir: gitWorkdir,
    commits,
    hasMoreLog,
    loadMoreLog,
    logSpec,
    setLogSpec: setAndStoreLogSpec,
    logSpecError,
    logLoaded,
    ensureLsp,
    lspHandle,
    noLsp,
    lspVersion,
    // Both return "" when the root is unknown, so callers can decline to open
    // rather than mint a tile whose path can never resolve.
    fileAssignment: (relPath) => {
      const a = abs(relPath);
      return a ? editorAssignment(connectionId, a) : "";
    },
    diffAssignment: (relPath, side) => {
      const a = abs(relPath);
      return a ? diffAssignment(connectionId, a, side) : "";
    },
    grep: async (query, opts) => {
      const r = rootPath();
      if (!r) return { files: [], truncated: false };
      return workspace.grep(connectionId, r, query, opts);
    },
    createFile: async (relPath) => {
      await fsForOps().writeFile(relPath, new Uint8Array(), {
        create: true,
        createParents: true,
      });
    },
    createDir: async (relPath) => {
      await fsForOps().mkdir(relPath, { createParents: true });
    },
    renamePath: async (from, to) => {
      await fsForOps().rename(from, to, { createParents: true });
    },
    removePath: async (relPath) => {
      await fsForOps().remove(relPath);
    },
  };

  function fsForOps(): FsSyncHandle {
    const h = rootFs();
    if (!h) throw new Error("File tree is not connected");
    return h;
  }
}

// ---------------------------------------------------------------------------
// Registry — ref-counted, idle-disposed sessions keyed by descriptor.key.
// Switching terminals releases the old session (kept warm for IDLE_MS) and
// acquires the new one; returning reuses the warm session with its tree,
// git state, and log intact.
// ---------------------------------------------------------------------------

const IDLE_MS = 30_000;

interface Entry {
  session: IdeSession;
  dispose: () => void;
  refs: number;
  idle: ReturnType<typeof setTimeout> | null;
}

const registry = new Map<string, Entry>();

function acquire(
  workspace: BlitWorkspace,
  desc: IdeSessionDescriptor,
): IdeSession {
  let entry = registry.get(desc.key);
  if (!entry) {
    let session!: IdeSession;
    const dispose = createRoot((d) => {
      session = buildSession(workspace, desc);
      return d;
    });
    entry = { session, dispose, refs: 0, idle: null };
    registry.set(desc.key, entry);
  }
  if (entry.idle !== null) {
    clearTimeout(entry.idle);
    entry.idle = null;
  }
  entry.refs++;
  return entry.session;
}

function release(key: string): void {
  const entry = registry.get(key);
  if (!entry) return;
  entry.refs--;
  if (entry.refs > 0) return;
  entry.idle = setTimeout(() => {
    registry.delete(key);
    entry.dispose();
  }, IDLE_MS);
}

/**
 * Track the active Ide session for a reactive descriptor. Acquires the
 * session for the current descriptor (reusing a warm one), releases it when
 * the descriptor changes or the owner is disposed. Returns `null` while no
 * root is selected.
 */
export function useIdeSession(
  workspace: BlitWorkspace,
  descriptor: Accessor<IdeSessionDescriptor | null>,
): Accessor<IdeSession | null> {
  let heldKey: string | null = null;

  // A memo, not an effect: it runs eagerly (so the session exists on first
  // read) and returns the session directly. The acquire/release ref-counting
  // is the side effect, guarded by `heldKey` so it fires only on key change —
  // when the key is unchanged we return the previous session untouched.
  const session = createMemo<IdeSession | null>((prev) => {
    const desc = descriptor();
    const nextKey = desc?.key ?? null;
    if (nextKey === heldKey) return prev ?? null;
    if (heldKey !== null) release(heldKey);
    heldKey = nextKey;
    return desc ? acquire(workspace, desc) : null;
  }, null);

  onCleanup(() => {
    if (heldKey !== null) release(heldKey);
  });

  return session;
}
