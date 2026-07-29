/**
 * Commit-DAG lane assignment for the log panel.
 *
 * Classic `git log --graph` layout: walk commits newest-first, keeping a set
 * of active lanes (each flowing toward the next commit it expects). For each
 * commit, the lanes that flow into it merge at its node; its first parent
 * continues in the node's lane and extra parents branch into new lanes. The
 * result is per-row geometry the panel renders as rails + a node.
 *
 * Lanes are reused (a freed column is reclaimed by a later branch) but never
 * shift mid-flow, so every rail is a straight vertical except the short edges
 * into and out of a node — which keeps rendering to one small SVG per row.
 *
 * The layout is resumable: {@link layoutGraph} carries the walk's live state
 * across calls, so appending a page of older commits lays out only the new
 * suffix — rows (and their objects) for pages already laid out are untouched,
 * which keeps their DOM alive. When the list changes any other way (head
 * moved, spec changed) the walk restarts, but rows whose geometry is
 * unchanged still reuse their previous objects, keyed by oid.
 */

export interface GraphCommit {
  /** Full hex oid. */
  oid: string;
  /** Full hex parent oids, first-parent first. */
  parents: string[];
}

export interface GraphRow {
  /** Column of this commit's node. */
  nodeCol: number;
  /** Columns of lanes passing straight through this row (unrelated branches). */
  through: number[];
  /** Columns of children merging into the node (edges top → node). */
  inCols: number[];
  /** Column each parent's lane occupies below the node (edges node → bottom). */
  outCols: number[];
  /** A parent lies far below (edge collapsed): draw a dashed down-stub. */
  suspendedOut: boolean;
  /** A child lies far above (its edge was collapsed): draw a dashed up-stub. */
  resumed: boolean;
}

export interface GraphLayoutState {
  /** The commit list these rows describe. Appends are detected by element
   *  identity against the next call's list. */
  commits: readonly GraphCommit[];
  rows: GraphRow[];
  /** Max simultaneous columns, for sizing the graph gutter. */
  columns: number;
  /** lane → the oid it flows toward (the walk's live frontier). */
  lanes: (string | null)[];
  /** oid → row index of every laid-out commit. */
  rowOf: Map<string, number>;
  /** oid → topmost child row expecting it (drives `resumed`). */
  minChildRow: Map<string, number>;
}

/** Rows a branch may sit idle before its edge is collapsed rather than held
 *  as a full-height rail occupying a column. */
const MAX_IDLE = 20;

const sameCols = (a: number[], b: number[]): boolean =>
  a.length === b.length && a.every((x, i) => x === b[i]);

const sameRow = (a: GraphRow, b: GraphRow): boolean =>
  a.nodeCol === b.nodeCol &&
  a.suspendedOut === b.suspendedOut &&
  a.resumed === b.resumed &&
  sameCols(a.through, b.through) &&
  sameCols(a.inCols, b.inCols) &&
  sameCols(a.outCols, b.outCols);

/** `commits` extends `prev.commits` (same objects, possibly more of them). */
const extendsPrev = (
  prev: GraphLayoutState,
  commits: readonly GraphCommit[],
): boolean => {
  if (commits.length < prev.commits.length) return false;
  for (let i = 0; i < prev.commits.length; i++) {
    if (commits[i] !== prev.commits[i]) return false;
  }
  return true;
};

/** Lay out rows [from, commits.length) into `state` (mutated in place). */
function walk(
  state: GraphLayoutState,
  commits: readonly GraphCommit[],
  from: number,
  maxIdle: number,
): void {
  const { lanes, rowOf, minChildRow } = state;

  // The whole suffix registers up front: `suspendedOut` needs to know
  // whether a parent is loaded anywhere below, not just above.
  for (let i = from; i < commits.length; i++) rowOf.set(commits[i].oid, i);

  const firstFree = (): number => {
    const i = lanes.indexOf(null);
    return i >= 0 ? i : lanes.length;
  };

  // Route `oid` to a lane: reuse the lane already flowing to it, else `prefer`
  // if free, else the first free column.
  const claim = (oid: string, prefer: number | null): number => {
    const existing = lanes.indexOf(oid);
    if (existing >= 0) return existing;
    let col: number;
    if (prefer != null && (prefer >= lanes.length || lanes[prefer] == null)) {
      col = prefer;
    } else {
      col = firstFree();
    }
    while (lanes.length <= col) lanes.push(null);
    lanes[col] = oid;
    return col;
  };

  for (let r = from; r < commits.length; r++) {
    const c = commits[r];
    const inCols: number[] = [];
    for (let i = 0; i < lanes.length; i++) {
      if (lanes[i] === c.oid) inCols.push(i);
    }
    const nodeCol = inCols.length > 0 ? inCols[0] : firstFree();

    // Lanes that keep flowing past this row (not merging into the node).
    const through: number[] = [];
    for (let i = 0; i < lanes.length; i++) {
      if (lanes[i] != null && !inCols.includes(i)) through.push(i);
    }

    // A child far above had this commit's edge collapsed: dashed up-stub.
    // Children precede parents in topological order, so every child's row
    // is already recorded by the time its parent is laid out.
    const childRow = minChildRow.get(c.oid);
    const resumed = childRow != null && r - childRow > maxIdle;

    // The merge-in lanes and the node lane are freed before reassigning.
    for (const i of inCols) lanes[i] = null;
    while (lanes.length <= nodeCol) lanes.push(null);
    lanes[nodeCol] = null;

    // Assign parents to lanes — but a parent that's loaded yet far below gets
    // its edge collapsed (a dashed stub) instead of a lane held idle for the
    // whole gap, so its column is reclaimed and the graph stays narrow.
    const outCols: number[] = [];
    let suspendedOut = false;
    c.parents.forEach((p, idx) => {
      if (!minChildRow.has(p)) minChildRow.set(p, r);
      const pr = rowOf.get(p);
      if (pr != null && pr - r > maxIdle) {
        suspendedOut = true;
        return;
      }
      outCols.push(claim(p, idx === 0 ? nodeCol : null));
    });

    let rowCols = Math.max(nodeCol + 1, lanes.length);
    for (const col of through) rowCols = Math.max(rowCols, col + 1);
    for (const col of inCols) rowCols = Math.max(rowCols, col + 1);
    for (const col of outCols) rowCols = Math.max(rowCols, col + 1);
    state.columns = Math.max(state.columns, rowCols);

    state.rows.push({
      nodeCol,
      through,
      inCols,
      outCols,
      suspendedOut,
      resumed,
    });
  }
}

/**
 * Lay out `commits`, resuming from `prev` when possible.
 *
 * - `commits` unchanged: returns `prev` as-is.
 * - `commits` appends to `prev.commits`: only the suffix is laid out; the
 *   existing rows keep their objects. (A lane a boundary row claimed for a
 *   then-unloaded parent stays claimed even if a fresh layout would have
 *   collapsed the edge — consistent with what's already on screen.)
 * - anything else: a fresh walk, reusing `prev`'s row objects for commits
 *   whose geometry is unchanged so their DOM survives.
 */
export function layoutGraph(
  prev: GraphLayoutState | null,
  commits: readonly GraphCommit[],
  maxIdle = MAX_IDLE,
): GraphLayoutState {
  if (prev && extendsPrev(prev, commits)) {
    if (commits.length === prev.commits.length) return prev;
    const state: GraphLayoutState = {
      commits,
      rows: prev.rows.slice(),
      columns: prev.columns,
      lanes: prev.lanes,
      rowOf: prev.rowOf,
      minChildRow: prev.minChildRow,
    };
    walk(state, commits, prev.commits.length, maxIdle);
    return state;
  }

  const state: GraphLayoutState = {
    commits,
    rows: [],
    columns: 1,
    lanes: [],
    rowOf: new Map(),
    minChildRow: new Map(),
  };
  walk(state, commits, 0, maxIdle);
  if (prev) {
    for (let i = 0; i < state.rows.length; i++) {
      const oldIdx = prev.rowOf.get(commits[i].oid);
      if (oldIdx == null) continue;
      const old = prev.rows[oldIdx];
      if (old && sameRow(old, state.rows[i])) state.rows[i] = old;
    }
  }
  return state;
}
