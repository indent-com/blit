/**
 * BSP layout DSL parser and serializer.
 *
 * Syntax:
 *   layout  = node
 *   node    = split | leaf
 *   split   = ("line" | "col" | "tabs") "(" entries ")"
 *   entries = entry ("," entry)*
 *   entry   = node [weight] ["@" fontSize]
 *   leaf    = identifier | quoted-string
 *   weight  = number
 *   fontSize = number ["px" | "pt" | "%"]
 *   identifier = [a-zA-Z_][a-zA-Z0-9_-]*
 *
 * Examples:
 *   shell
 *   line(chat 2, col(shell, tail, htop))
 *   line(editor 3 @14, col(shell @11, logs))
 *   tabs(shell, htop, logs)
 *   line(editor 2, tabs(shell, logs))
 */

export type BSPNode = BSPSplit | BSPLeaf;

export interface BSPSplit {
  type: "split";
  direction: "horizontal" | "vertical" | "tabs";
  children: BSPChild[];
}

export interface BSPChild {
  node: BSPNode;
  weight: number;
}

export interface BSPLeaf {
  type: "leaf";
  tag: string;
  /** Raw font size, e.g. 14, "12px", "13pt", "80%" */
  fontSize?: number | string;
}

export class DSLParseError extends Error {
  constructor(
    message: string,
    public readonly offset: number,
  ) {
    super(message);
  }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

export function parseDSL(input: string): { root: BSPNode; weight: number } {
  const trimmed = input.trim();
  if (!trimmed) throw new DSLParseError("Empty layout", 0);

  let pos = 0;

  function peek(): string {
    return trimmed[pos] ?? "";
  }

  function skipWhitespace(): void {
    while (pos < trimmed.length && /\s/.test(trimmed[pos])) pos++;
  }

  function expect(ch: string): void {
    skipWhitespace();
    if (trimmed[pos] !== ch) {
      throw new DSLParseError(`Expected '${ch}' at position ${pos}`, pos);
    }
    pos++;
  }

  function parseNumber(): number {
    skipWhitespace();
    const start = pos;
    while (pos < trimmed.length && /[0-9.]/.test(trimmed[pos])) pos++;
    if (pos === start) throw new DSLParseError(`Expected number at position ${pos}`, pos);
    const n = Number(trimmed.slice(start, pos));
    if (!Number.isFinite(n) || n <= 0) {
      throw new DSLParseError(`Invalid number at position ${start}`, start);
    }
    return n;
  }

  function parseIdentifier(): string {
    skipWhitespace();
    // Quoted string
    if (peek() === '"' || peek() === "'") {
      const quote = trimmed[pos];
      pos++;
      let value = "";
      while (pos < trimmed.length && trimmed[pos] !== quote) {
        if (trimmed[pos] === "\\" && pos + 1 < trimmed.length) {
          pos++;
          value += trimmed[pos];
        } else {
          value += trimmed[pos];
        }
        pos++;
      }
      if (pos >= trimmed.length) throw new DSLParseError(`Unterminated string at position ${pos}`, pos);
      pos++; // skip closing quote
      return value;
    }
    const start = pos;
    while (pos < trimmed.length && /[a-zA-Z0-9_-]/.test(trimmed[pos])) pos++;
    if (pos === start) throw new DSLParseError(`Expected identifier at position ${pos}`, pos);
    return trimmed.slice(start, pos);
  }

  function parseFontSize(): number | string {
    const n = parseNumber();
    const unitStart = pos;
    if (pos < trimmed.length && /[a-z%]/.test(trimmed[pos])) {
      while (pos < trimmed.length && /[a-z%]/.test(trimmed[pos])) pos++;
      const unit = trimmed.slice(unitStart, pos);
      if (unit === "px" || unit === "pt" || unit === "%") {
        return `${n}${unit}`;
      }
      throw new DSLParseError(`Unknown font size unit '${unit}' at position ${unitStart}`, unitStart);
    }
    return n;
  }

  function parseEntry(): { node: BSPNode; weight: number; fontSize?: number | string } {
    const node = parseNode();
    skipWhitespace();

    let weight = 1;
    let fontSize: number | string | undefined;

    // Parse optional weight (number not preceded by @)
    if (pos < trimmed.length && /[0-9]/.test(peek())) {
      weight = parseNumber();
    }

    // Parse optional @fontSize
    skipWhitespace();
    if (peek() === "@") {
      pos++;
      fontSize = parseFontSize();
    }

    return { node, weight, fontSize };
  }

  function parseNode(): BSPNode {
    skipWhitespace();
    const start = pos;
    const id = parseIdentifier();

    if ((id === "line" || id === "col" || id === "tabs") && (skipWhitespace(), peek() === "(")) {
      const direction = id === "line" ? "horizontal" : id === "col" ? "vertical" : "tabs";
      expect("(");
      const children: BSPChild[] = [];
      const firstEntry = parseEntry();

      // Apply fontSize to leaf nodes
      if (firstEntry.node.type === "leaf" && firstEntry.fontSize != null) {
        firstEntry.node.fontSize = firstEntry.fontSize;
      }
      children.push({ node: firstEntry.node, weight: firstEntry.weight });

      skipWhitespace();
      while (peek() === ",") {
        pos++;
        const entry = parseEntry();
        if (entry.node.type === "leaf" && entry.fontSize != null) {
          entry.node.fontSize = entry.fontSize;
        }
        children.push({ node: entry.node, weight: entry.weight });
        skipWhitespace();
      }

      expect(")");

      if (children.length < 2) {
        throw new DSLParseError(`Split needs at least 2 children at position ${start}`, start);
      }

      return { type: "split", direction, children };
    }

    // Not a split — it's a leaf
    return { type: "leaf", tag: id };
  }

  const entry = parseEntry();
  if (entry.node.type === "leaf" && entry.fontSize != null) {
    entry.node.fontSize = entry.fontSize;
  }

  skipWhitespace();
  if (pos < trimmed.length) {
    throw new DSLParseError(`Unexpected '${trimmed[pos]}' at position ${pos}`, pos);
  }

  return { root: entry.node, weight: entry.weight };
}

// ---------------------------------------------------------------------------
// Serializer
// ---------------------------------------------------------------------------

function serializeNode(node: BSPNode, weight: number, fontSize?: number | string): string {
  let s: string;
  if (node.type === "leaf") {
    s = /^[a-zA-Z_][a-zA-Z0-9_-]*$/.test(node.tag) ? node.tag : `"${node.tag.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;

    fontSize = fontSize ?? node.fontSize;
  } else {
    const keyword = node.direction === "horizontal" ? "line" : node.direction === "vertical" ? "col" : "tabs";
    const inner = node.children
      .map((c) => serializeNode(c.node, c.weight))
      .join(", ");
    s = `${keyword}(${inner})`;
  }
  if (weight !== 1) s += ` ${weight}`;
  if (fontSize != null) s += ` @${fontSize}`;
  return s;
}

export function serializeDSL(root: BSPNode, weight = 1, fontSize?: number | string): string {
  return serializeNode(root, weight, fontSize);
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

export function collectTags(node: BSPNode): string[] {
  if (node.type === "leaf") return [node.tag];
  return node.children.flatMap((c) => collectTags(c.node));
}

export function leafCount(node: BSPNode): number {
  if (node.type === "leaf") return 1;
  return node.children.reduce((sum, c) => sum + leafCount(c.node), 0);
}
