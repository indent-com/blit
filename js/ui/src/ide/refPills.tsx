/**
 * Ref pills — the refs pointing at a commit, rendered as compact chips.
 * Shared by the commit log (per-row decorations) and the commit viewer
 * (header). Data comes from the pushed `GIT_STATE` snapshot's STATE_REF
 * records (docs/design/git.md): local branches, tags, remotes, plus a
 * detached HEAD; annotated tags decorate the commit they peel to.
 */

import { For, Show } from "solid-js";
import {
  GIT_HEAD_DETACHED,
  GIT_HEAD_UNBORN,
  GIT_REF_PEELED_VALID,
  gitOidHex,
  type GitStateMirror,
} from "@blit-sh/core";
import type { Theme, UIScale } from "../theme";

export type RefPill = {
  label: string;
  /** Full ref name, for the tooltip. */
  full: string;
  /** "op" marks an in-progress operation's refs — gitdir pseudo-refs
   *  (MERGE_HEAD, REBASE_HEAD, …) and refs/bisect/*. */
  kind: "head" | "op" | "branch" | "tag" | "remote";
};

/** Hex commit oid → the pills decorating it, ordered head → branches →
 *  tags → remotes. */
export function collectRefPills(
  gs: GitStateMirror,
  oidFormat: number | undefined,
): Map<string, RefPill[]> {
  const out = new Map<string, RefPill[]>();
  const push = (oidHex: string, pill: RefPill) => {
    let list = out.get(oidHex);
    if (!list) {
      list = [];
      out.set(oidHex, list);
    }
    list.push(pill);
  };
  const head = gs.head;
  const headBranch =
    head && !(head.flags & (GIT_HEAD_DETACHED | GIT_HEAD_UNBORN))
      ? head.name
      : null;
  for (const [name, ref] of gs.refs) {
    const target = ref.flags & GIT_REF_PEELED_VALID ? ref.peeled : ref.oid;
    const oidHex = gitOidHex(target, oidFormat);
    let pill: RefPill | null = null;
    let stripped: string;
    if ((stripped = name.replace(/^refs\/heads\//, "")) !== name) {
      pill = {
        label: stripped,
        full: name,
        kind: name === headBranch ? "head" : "branch",
      };
    } else if ((stripped = name.replace(/^refs\/tags\//, "")) !== name) {
      pill = { label: stripped, full: name, kind: "tag" };
    } else if ((stripped = name.replace(/^refs\/remotes\//, "")) !== name) {
      pill = { label: stripped, full: name, kind: "remote" };
    } else if ((stripped = name.replace(/^refs\/bisect\//, "")) !== name) {
      pill = { label: `bisect/${stripped}`, full: name, kind: "op" };
    } else if (/^[A-Z_]+(#\d+)?$/.test(name)) {
      // Gitdir pseudo-refs (MERGE_HEAD, REBASE_HEAD, MERGE_HEAD#2 for an
      // octopus, …) — the server streams them only while an operation is
      // in progress (docs/design/git.md).
      pill = { label: name, full: name, kind: "op" };
    }
    // Everything else (refs/stash, notes, …) stays undecorated.
    if (pill) push(oidHex, pill);
  }
  if (head && head.flags & GIT_HEAD_DETACHED) {
    push(gitOidHex(head.oid, oidFormat), {
      label: "HEAD",
      full: "HEAD",
      kind: "head",
    });
  }
  const order: RefPill["kind"][] = ["head", "op", "branch", "tag", "remote"];
  for (const list of out.values())
    list.sort((a, b) => order.indexOf(a.kind) - order.indexOf(b.kind));
  return out;
}

export function RefPills(props: {
  pills: RefPill[];
  theme: Theme;
  scale: UIScale;
  /** Show at most this many pills, then a +N spillover (default 3). */
  max?: number;
}) {
  const max = () => props.max ?? 3;
  const color = (kind: RefPill["kind"]): string =>
    kind === "op"
      ? props.theme.error
      : kind === "tag"
        ? props.theme.warning
        : kind === "remote"
          ? props.theme.dimFg
          : props.theme.accent;
  return (
    <span
      style={{
        // Siblings after the pills (e.g. the DAG rails) keep their
        // place: when the row runs out of width the pills shrink and
        // clip, never their neighbors.
        "flex-shrink": 1,
        "min-width": 0,
        overflow: "hidden",
        display: "flex",
        gap: "3px",
        "align-items": "center",
      }}
    >
      <For each={props.pills.slice(0, max())}>
        {(pill) => (
          <span
            title={pill.full}
            style={{
              "font-size": `${props.scale.xs}px`,
              "line-height": 1.4,
              padding: "0 4px",
              "border-radius": "3px",
              color: color(pill.kind),
              border: `1px solid color-mix(in srgb, ${color(pill.kind)} 45%, transparent)`,
              background: `color-mix(in srgb, ${color(pill.kind)} ${pill.kind === "head" ? 28 : 12}%, transparent)`,
              "font-weight": pill.kind === "head" ? 700 : 400,
              "max-width": "12em",
              overflow: "hidden",
              "text-overflow": "ellipsis",
            }}
          >
            {pill.label}
          </span>
        )}
      </For>
      <Show when={props.pills.length > max()}>
        <span
          title={props.pills
            .slice(max())
            .map((r) => r.full)
            .join("\n")}
          style={{
            "font-size": `${props.scale.xs}px`,
            color: props.theme.dimFg,
          }}
        >
          +{props.pills.length - max()}
        </span>
      </Show>
    </span>
  );
}
