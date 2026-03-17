import { describe, it, expect } from "vitest";
import {
  parseDSL,
  serializeDSL,
  collectTags,
  leafCount,
  DSLParseError,
} from "../bsp/dsl";
import type { BSPNode, BSPLeaf, BSPSplit } from "../bsp/dsl";

describe("parseDSL", () => {
  it("parses a single leaf", () => {
    const { root, weight } = parseDSL("shell");
    expect(root.type).toBe("leaf");
    expect((root as BSPLeaf).tag).toBe("shell");
    expect(weight).toBe(1);
  });

  it("parses a leaf with weight", () => {
    const { root, weight } = parseDSL("editor 3");
    expect(root.type).toBe("leaf");
    expect((root as BSPLeaf).tag).toBe("editor");
    expect(weight).toBe(3);
  });

  it("parses a horizontal split", () => {
    const { root } = parseDSL("line(a, b)");
    expect(root.type).toBe("split");
    const split = root as BSPSplit;
    expect(split.direction).toBe("horizontal");
    expect(split.children).toHaveLength(2);
    expect((split.children[0].node as BSPLeaf).tag).toBe("a");
    expect((split.children[1].node as BSPLeaf).tag).toBe("b");
  });

  it("parses a vertical split", () => {
    const { root } = parseDSL("col(x, y)");
    expect(root.type).toBe("split");
    expect((root as BSPSplit).direction).toBe("vertical");
  });

  it("parses tabs", () => {
    const { root } = parseDSL("tabs(a, b, c)");
    expect(root.type).toBe("split");
    const split = root as BSPSplit;
    expect(split.direction).toBe("tabs");
    expect(split.children).toHaveLength(3);
  });

  it("parses nested splits", () => {
    const { root } = parseDSL("line(editor 2, col(shell, logs))");
    expect(root.type).toBe("split");
    const split = root as BSPSplit;
    expect(split.children[0].weight).toBe(2);
    expect(split.children[1].node.type).toBe("split");
    const inner = split.children[1].node as BSPSplit;
    expect(inner.direction).toBe("vertical");
    expect(inner.children).toHaveLength(2);
  });

  it("parses a leaf with command", () => {
    const { root } = parseDSL('shell="cd /src && make"');
    expect(root.type).toBe("leaf");
    const leaf = root as BSPLeaf;
    expect(leaf.tag).toBe("shell");
    expect(leaf.command).toBe("cd /src && make");
  });

  it("parses a leaf with fontSize (bare number)", () => {
    const { root } = parseDSL("editor @14");
    expect(root.type).toBe("leaf");
    expect((root as BSPLeaf).fontSize).toBe(14);
  });

  it("parses a leaf with fontSize px unit", () => {
    const { root } = parseDSL("editor @12px");
    expect((root as BSPLeaf).fontSize).toBe("12px");
  });

  it("parses a leaf with fontSize pt unit", () => {
    const { root } = parseDSL("editor @13pt");
    expect((root as BSPLeaf).fontSize).toBe("13pt");
  });

  it("parses a leaf with fontSize % unit", () => {
    const { root } = parseDSL("editor @80%");
    expect((root as BSPLeaf).fontSize).toBe("80%");
  });

  it("parses labels on entries", () => {
    const { root } = parseDSL('tabs("Editor": shell, "Term": logs)');
    const split = root as BSPSplit;
    expect(split.children[0].label).toBe("Editor");
    expect(split.children[1].label).toBe("Term");
  });

  it("parses unquoted labels", () => {
    const { root } = parseDSL("tabs(myLabel: shell, other: logs)");
    const split = root as BSPSplit;
    expect(split.children[0].label).toBe("myLabel");
    expect(split.children[1].label).toBe("other");
  });

  it("does not treat line/col/tabs as labels", () => {
    const { root } = parseDSL("tabs(line(a, b), col(c, d))");
    const split = root as BSPSplit;
    expect(split.children[0].label).toBeUndefined();
    expect(split.children[0].node.type).toBe("split");
  });

  it("parses quoted identifiers with escapes", () => {
    const { root } = parseDSL('"my\\"tag"');
    expect(root.type).toBe("leaf");
    expect((root as BSPLeaf).tag).toBe('my"tag');
  });

  it("throws on empty input", () => {
    expect(() => parseDSL("")).toThrow(DSLParseError);
    expect(() => parseDSL("   ")).toThrow(DSLParseError);
  });

  it("throws on split with single child", () => {
    expect(() => parseDSL("line(a)")).toThrow("at least 2 children");
  });

  it("throws on trailing garbage", () => {
    expect(() => parseDSL("shell extra")).toThrow(DSLParseError);
  });

  it("throws on unknown font unit", () => {
    expect(() => parseDSL("shell @14em")).toThrow("Unknown font size unit");
  });

  it("throws on command on split node", () => {
    expect(() => parseDSL('line(a, b)="cmd"')).toThrow(
      "command can only be applied to leaf",
    );
  });

  it("throws on unterminated string", () => {
    expect(() => parseDSL('"unterminated')).toThrow("Unterminated string");
  });

  it("parses weight + command + fontSize together", () => {
    const { root, weight } = parseDSL('editor 2 ="vim" @14');
    expect(root.type).toBe("leaf");
    const leaf = root as BSPLeaf;
    expect(leaf.tag).toBe("editor");
    expect(weight).toBe(2);
    expect(leaf.command).toBe("vim");
    expect(leaf.fontSize).toBe(14);
  });
});

describe("serializeDSL", () => {
  it("serializes a single leaf", () => {
    const node: BSPNode = { type: "leaf", tag: "shell" };
    expect(serializeDSL(node)).toBe("shell");
  });

  it("serializes with weight", () => {
    const node: BSPNode = { type: "leaf", tag: "shell" };
    expect(serializeDSL(node, 3)).toBe("shell 3");
  });

  it("serializes a split", () => {
    const node: BSPNode = {
      type: "split",
      direction: "horizontal",
      children: [
        { node: { type: "leaf", tag: "a" }, weight: 1 },
        { node: { type: "leaf", tag: "b" }, weight: 1 },
      ],
    };
    expect(serializeDSL(node)).toBe("line(a, b)");
  });

  it("serializes command with escaping", () => {
    const node: BSPNode = {
      type: "leaf",
      tag: "shell",
      command: 'echo "hi"',
    };
    expect(serializeDSL(node)).toBe('shell="echo \\"hi\\""');
  });

  it("serializes fontSize", () => {
    const node: BSPNode = { type: "leaf", tag: "editor", fontSize: 14 };
    expect(serializeDSL(node)).toBe("editor @14");
  });

  it("quotes tags with special characters", () => {
    const node: BSPNode = { type: "leaf", tag: "my shell" };
    expect(serializeDSL(node)).toBe('"my shell"');
  });
});

describe("parseDSL / serializeDSL round-trip", () => {
  const cases = [
    "shell",
    "line(a, b)",
    "col(x 2, y)",
    "tabs(a, b, c)",
    "line(editor 2, col(shell, logs))",
  ];

  for (const dsl of cases) {
    it(`round-trips: ${dsl}`, () => {
      const { root, weight } = parseDSL(dsl);
      const serialized = serializeDSL(root, weight);
      const reparsed = parseDSL(serialized);
      expect(reparsed.root).toEqual(root);
      expect(reparsed.weight).toBe(weight);
    });
  }
});

describe("collectTags", () => {
  it("returns single tag for leaf", () => {
    const { root } = parseDSL("shell");
    expect(collectTags(root)).toEqual(["shell"]);
  });

  it("returns all tags for nested split", () => {
    const { root } = parseDSL("line(a, col(b, c))");
    expect(collectTags(root)).toEqual(["a", "b", "c"]);
  });

  it("preserves duplicates", () => {
    const { root } = parseDSL("line(shell, shell)");
    expect(collectTags(root)).toEqual(["shell", "shell"]);
  });
});

describe("leafCount", () => {
  it("returns 1 for a leaf", () => {
    const { root } = parseDSL("shell");
    expect(leafCount(root)).toBe(1);
  });

  it("returns 2 for simple split", () => {
    const { root } = parseDSL("line(a, b)");
    expect(leafCount(root)).toBe(2);
  });

  it("returns 4 for a grid", () => {
    const { root } = parseDSL("col(line(a, b), line(c, d))");
    expect(leafCount(root)).toBe(4);
  });
});
