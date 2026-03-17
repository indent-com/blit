#!/usr/bin/env python3
"""Read cargo-llvm-cov JSON export and produce a markdown summary table grouped by crate."""

import json
import sys
from collections import defaultdict


def main():
    with open("coverage.json") as f:
        data = json.load(f)

    files = data["data"][0]["files"]
    totals = data["data"][0]["totals"]

    crates = defaultdict(lambda: {"lines": [0, 0], "fns": [0, 0], "regions": [0, 0]})

    for f in files:
        path = f["filename"]
        parts = path.split("/")
        if "crates" in parts:
            idx = parts.index("crates")
            crate = parts[idx + 1] if idx + 1 < len(parts) else "other"
        else:
            crate = "other"

        s = f["summary"]
        crates[crate]["lines"][0] += s["lines"]["covered"]
        crates[crate]["lines"][1] += s["lines"]["count"]
        crates[crate]["fns"][0] += s["functions"]["covered"]
        crates[crate]["fns"][1] += s["functions"]["count"]
        crates[crate]["regions"][0] += s["regions"]["covered"]
        crates[crate]["regions"][1] += s["regions"]["count"]

    def pct(covered, total):
        if total == 0:
            return "-"
        return f"{covered / total * 100:.1f}%"

    def cell(covered, total):
        if total == 0:
            return "-"
        return f"{pct(covered, total)} ({covered}/{total})"

    lines = [
        "### Coverage",
        "",
        "| Crate | Lines | Functions | Regions |",
        "|:------|------:|----------:|--------:|",
    ]

    for name in sorted(crates):
        c = crates[name]
        lines.append(
            f"| {name} | {cell(*c['lines'])} | {cell(*c['fns'])} | {cell(*c['regions'])} |"
        )

    tl = totals["lines"]
    tf = totals["functions"]
    tr = totals["regions"]
    lines.append(
        f"| **Total** | **{cell(tl['covered'], tl['count'])}** "
        f"| **{cell(tf['covered'], tf['count'])}** "
        f"| **{cell(tr['covered'], tr['count'])}** |"
    )

    output = "\n".join(lines) + "\n"

    with open("coverage-summary.txt", "w") as out:
        out.write(output)

    print(output)


if __name__ == "__main__":
    main()
