/**
 * BSP layout DSL parser and serializer.
 *
 * Syntax:
 *   layout  = node
 *   node    = split | leaf
 *   split   = ("line" | "col" | "tabs") "(" entries ")"
 *   entries = entry ("," entry)*
 *   entry   = [label ":"] node [weight] ["=" command] ["@" fontSize]
 *   label   = identifier | quoted-string
 *   leaf    = identifier | quoted-string
 *   command = identifier | quoted-string
 *   weight  = number
 *   fontSize = number ["px" | "pt" | "%"]
 *   identifier = [a-zA-Z_][a-zA-Z0-9_-]*
 *
 * Examples:
 *   shell
 *   line(chat 2, col(shell, tail, htop))
 *   line(editor 3 @14, col(shell @11, logs))
 *   tabs(shell, htop, logs)
 *   tabs("Editor": col(a, b), "Terminal": col(c, d))
 *   line(editor 2, tabs(shell, logs))
 *   col(htop="htop", shell="cd /src && make watch")
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
  label?: string;
}

export interface BSPLeaf {
  type: "leaf";
  tag: string;
  /** Shell command to run when the pane is created. */
  command?: string;
  /** Raw font size, e.g. 14, "12px", "13pt", "80%" */
  fontSize?: number | string;
}

const SPECIAL_CHARS = /[\s(),@'"\\:=]/;

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
    if (pos === start)
      throw new DSLParseError(`Expected number at position ${pos}`, pos);
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
      if (pos >= trimmed.length)
        throw new DSLParseError(`Unterminated string at position ${pos}`, pos);
      pos++; // skip closing quote
      return value;
    }
    const start = pos;
    while (pos < trimmed.length && !SPECIAL_CHARS.test(trimmed[pos])) pos++;
    if (pos === start)
      throw new DSLParseError(`Expected identifier at position ${pos}`, pos);
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
      throw new DSLParseError(
        `Unknown font size unit '${unit}' at position ${unitStart}`,
        unitStart,
      );
    }
    return n;
  }

  function parseEntry(): BSPChild {
    let label: string | undefined;

    skipWhitespace();
    const savedPos = pos;
    if (peek() === '"' || peek() === "'") {
      const candidate = parseIdentifier();
      skipWhitespace();
      if (peek() === ":") {
        pos++;
        label = candidate;
      } else {
        pos = savedPos;
      }
    } else if (/[a-zA-Z_]/.test(peek())) {
      const candidate = parseIdentifier();
      skipWhitespace();
      if (
        peek() === ":" &&
        candidate !== "line" &&
        candidate !== "col" &&
        candidate !== "tabs"
      ) {
        pos++;
        label = candidate;
      } else {
        pos = savedPos;
      }
    }

    const node = parseNode();
    skipWhitespace();

    let weight = 1;
    if (pos < trimmed.length && /[0-9]/.test(peek())) {
      weight = parseNumber();
    }

    skipWhitespace();
    if (peek() === "=") {
      pos++;
      const command = parseIdentifier();
      if (node.type !== "leaf") {
        throw new DSLParseError(
          `command can only be applied to leaf nodes at position ${pos}`,
          pos,
        );
      }
      node.command = command;
    }

    skipWhitespace();
    if (peek() === "@") {
      pos++;
      const fontSize = parseFontSize();
      if (node.type !== "leaf") {
        throw new DSLParseError(
          `fontSize can only be applied to leaf nodes at position ${pos}`,
          pos,
        );
      }
      node.fontSize = fontSize;
    }

    return { node, weight, ...(label != null && { label }) };
  }

  function parseNode(): BSPNode {
    skipWhitespace();
    const start = pos;
    const id = parseIdentifier();

    if (
      (id === "line" || id === "col" || id === "tabs") &&
      (skipWhitespace(), peek() === "(")
    ) {
      const direction =
        id === "line" ? "horizontal" : id === "col" ? "vertical" : "tabs";
      expect("(");
      const children: BSPChild[] = [parseEntry()];

      skipWhitespace();
      while (peek() === ",") {
        pos++;
        children.push(parseEntry());
        skipWhitespace();
      }

      expect(")");

      if (children.length < 2) {
        throw new DSLParseError(
          `Split needs at least 2 children at position ${start}`,
          start,
        );
      }

      return { type: "split", direction, children };
    }

    return { type: "leaf", tag: id };
  }

  const entry = parseEntry();

  skipWhitespace();
  if (pos < trimmed.length) {
    throw new DSLParseError(
      `Unexpected '${trimmed[pos]}' at position ${pos}`,
      pos,
    );
  }

  return { root: entry.node, weight: entry.weight };
}

// ---------------------------------------------------------------------------
// Serializer
// ---------------------------------------------------------------------------

function quoteIfNeeded(value: string): string {
  return value.length > 0 && !SPECIAL_CHARS.test(value)
    ? value
    : `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

function serializeNode(node: BSPNode, weight: number, label?: string): string {
  let s: string;
  if (node.type === "leaf") {
    s = quoteIfNeeded(node.tag);
  } else {
    const keyword =
      node.direction === "horizontal"
        ? "line"
        : node.direction === "vertical"
          ? "col"
          : "tabs";
    const inner = node.children
      .map((c) => serializeNode(c.node, c.weight, c.label))
      .join(", ");
    s = `${keyword}(${inner})`;
  }
  if (weight !== 1) s += ` ${weight}`;
  if (node.type === "leaf" && node.command != null) {
    s += `="${node.command.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
  }
  if (node.type === "leaf" && node.fontSize != null) s += ` @${node.fontSize}`;
  if (label != null) s = `${quoteIfNeeded(label)}: ${s}`;
  return s;
}

export function serializeDSL(root: BSPNode, weight = 1): string {
  return serializeNode(root, weight);
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
