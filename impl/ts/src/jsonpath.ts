// The `jsonpath` type's compiler + canonical renderer (spec/design/jsonpath.md, slice P1a).
//
// P1a is the LITERAL-ONLY surface (like J0 for json): the `jsonpath` scalar type, the
// `'…'::jsonpath` / `jsonpath '…'` literal cast (compiled at resolve), and the canonical render
// (`$.a` → `$."a"`, `lax` omitted, `strict` kept). The structural-accessor subset is parsed here
// ($, `.key`, `.*`, `[subscripts]`, `[*]`, numeric / `last` indices, `to` slices, lax/strict mode);
// the eval engine, filters, item methods, arithmetic, `like_regex`, and `$name` variables are a
// deferred P1b follow-on (a valid-PG path using one → `0A000` at compile). A malformed path is
// `42601`. The compiled program is a pure function of the source — kept byte-identical cross-core by
// the conformance suite (CLAUDE.md §5: a hand-written parser, never codegenned).
//
// This is a port of impl/rust/src/jsonpath.rs. The Rust parser walks bytes; this one walks
// JS string characters, which is byte-equivalent for the boundary/escape rules: `eatKeyword`'s
// whole-word check (next char is not alphanumeric/`_`), the quoted-string JSON escapes, and the
// member-key JSON-string escaping all match the Rust version.

import { engineError, type EngineError } from "./errors.ts";
import type { JsonNode } from "./json.ts";

// A subscript index: a non-negative integer literal or the `last` sentinel.
export type Index = { kind: "number"; value: number } | { kind: "last" };

// One subscript: a single index or an `i to j` slice.
export type Subscript = { kind: "index"; index: Index } | { kind: "slice"; from: Index; to: Index };

// One accessor step.
export type Step =
  | { kind: "member"; key: string } // `.key` — a member accessor (the key, unescaped).
  | { kind: "wildcardMember" } // `.*` — the wildcard member accessor.
  | { kind: "subscripts"; subs: Subscript[] } // `[s, …]` — one or more subscripts.
  | { kind: "wildcardElement" }; // `[*]` — the wildcard element accessor.

// A compiled jsonpath (the structural-accessor subset, P1a).
export type JsonPath = { strict: boolean; steps: Step[] };

// A jsonpath construct that is valid in PostgreSQL but not yet supported by jed (a deferred P1b
// follow-on): `0A000`, a documented divergence.
function unsupported(what: string): EngineError {
  return engineError("feature_not_supported", `jsonpath ${what} is not supported yet`);
}

// A malformed jsonpath literal: `42601` (PostgreSQL's syntax-error class for a bad path literal).
function malformed(detail: string): EngineError {
  return engineError("syntax_error", `invalid jsonpath: ${detail}`);
}

function isMemberStart(c: string): boolean {
  return (c >= "a" && c <= "z") || (c >= "A" && c <= "Z") || c === "_";
}

function isMemberCont(c: string): boolean {
  return (
    (c >= "a" && c <= "z") ||
    (c >= "A" && c <= "Z") ||
    (c >= "0" && c <= "9") ||
    c === "_" ||
    c === "$"
  );
}

function isAsciiDigit(c: string): boolean {
  return c >= "0" && c <= "9";
}

function isAsciiAlnum(c: string): boolean {
  return (c >= "a" && c <= "z") || (c >= "A" && c <= "Z") || (c >= "0" && c <= "9");
}

function isAsciiWhitespace(c: string): boolean {
  // Rust's char::is_ascii_whitespace: space (0x20), tab (0x09), LF (0x0A), FF (0x0C), CR
  // (0x0D). Note 0x0B (vertical tab) is NOT included — matching Rust exactly.
  return c === " " || c === "\t" || c === "\n" || c === "\f" || c === "\r";
}

// Parser walks the source string char by char (mirrors Rust's byte cursor — see the module note).
class Parser {
  private readonly s: string;
  private i: number;

  constructor(src: string) {
    this.s = src;
    this.i = 0;
  }

  private peek(): string | undefined {
    return this.i < this.s.length ? this.s[this.i] : undefined;
  }

  private eat(c: string): boolean {
    if (this.peek() === c) {
      this.i += 1;
      return true;
    }
    return false;
  }

  skipWs(): void {
    while (this.i < this.s.length && isAsciiWhitespace(this.s[this.i]!)) {
      this.i += 1;
    }
  }

  // Consume `kw` if it appears as a whole WORD at the cursor — i.e. the following char is not an
  // identifier-continuation character (so `last]`, `to `, `strict $` all match, but `lastfoo` does
  // not).
  eatKeyword(kw: string): boolean {
    if (this.s.startsWith(kw, this.i)) {
      const after = this.i + kw.length < this.s.length ? this.s[this.i + kw.length] : undefined;
      if (after === undefined || !(isAsciiAlnum(after) || after === "_")) {
        this.i += kw.length;
        return true;
      }
    }
    return false;
  }

  // Compile a jsonpath source string (P1a structural subset). Malformed → `42601`; a valid-PG but
  // unsupported construct → `0A000`.
  compile(): JsonPath {
    this.skipWs();
    // Optional mode word: `strict` / `lax` (default lax).
    let strict: boolean;
    if (this.eatKeyword("strict")) {
      strict = true;
    } else {
      this.eatKeyword("lax");
      strict = false;
    }
    this.skipWs();
    if (!this.eat("$")) {
      // `@`, a variable, or a bare literal as a top-level path expression — the filter / scalar
      // path-expression surface (a P1b follow-on).
      throw unsupported("expressions other than a `$`-rooted accessor path");
    }
    // `$name` — a path variable (the `$` immediately followed by a name char / quote) is a P1b
    // follow-on (the bound-variable `vars` surface).
    const after$ = this.peek();
    if (after$ !== undefined && (isMemberStart(after$) || after$ === '"')) {
      throw unsupported("path variables `$name`");
    }
    const steps: Step[] = [];
    for (;;) {
      this.skipWs();
      const c = this.peek();
      if (c === undefined) break;
      if (c === ".") {
        this.i += 1;
        if (this.eat("*")) {
          steps.push({ kind: "wildcardMember" });
        } else {
          const next = this.peek();
          if (next !== undefined && (next === '"' || isMemberStart(next))) {
            const m = this.parseMember();
            // `.identifier(` is an item-method call (a P1b follow-on); a bare identifier is a
            // member accessor.
            if (this.peek() === "(") {
              throw unsupported("item methods");
            }
            steps.push({ kind: "member", key: m });
          } else {
            // `$.` with nothing (or a non-member) after it is malformed.
            throw malformed("expected a member name after `.`");
          }
        }
      } else if (c === "[") {
        this.i += 1;
        this.skipWs();
        if (this.eat("*")) {
          this.skipWs();
          if (!this.eat("]")) {
            throw malformed("expected `]` after `[*`");
          }
          steps.push({ kind: "wildcardElement" });
        } else {
          steps.push({ kind: "subscripts", subs: this.parseSubscripts() });
        }
      } else if (c === "?") {
        throw unsupported("filter expressions `?(…)`");
      } else if (
        // Arithmetic / comparison operators on a path expression are a P1b follow-on.
        c === "+" ||
        c === "-" ||
        c === "*" ||
        c === "/" ||
        c === "%" ||
        c === "=" ||
        c === "<" ||
        c === ">" ||
        c === "!" ||
        c === "&" ||
        c === "|"
      ) {
        throw unsupported("path arithmetic / predicate operators");
      } else {
        throw malformed("unexpected character in path");
      }
    }
    return { strict, steps };
  }

  // Parse a member key after `.`: a bare identifier or a `"…"` quoted string.
  private parseMember(): string {
    if (this.peek() === '"') {
      return this.parseQuoted();
    }
    const start = this.i;
    while (this.i < this.s.length && isMemberCont(this.s[this.i]!)) {
      this.i += 1;
    }
    if (this.i === start) {
      throw malformed("empty member name");
    }
    return this.s.slice(start, this.i);
  }

  // Parse a `"…"` jsonpath string (JSON escapes).
  private parseQuoted(): string {
    this.i += 1; // opening "
    let out = "";
    for (;;) {
      const c = this.peek();
      if (c === undefined) {
        throw malformed("unterminated string");
      }
      if (c === '"') {
        this.i += 1;
        return out;
      }
      if (c === "\\") {
        this.i += 1;
        const e = this.peek();
        switch (e) {
          case '"':
            out += '"';
            break;
          case "\\":
            out += "\\";
            break;
          case "/":
            out += "/";
            break;
          case "n":
            out += "\n";
            break;
          case "r":
            out += "\r";
            break;
          case "t":
            out += "\t";
            break;
          case "b":
            out += "\b";
            break;
          case "f":
            out += "\f";
            break;
          case "u": {
            const hex = this.s.slice(this.i + 1, this.i + 5);
            if (hex.length !== 4 || !/^[0-9a-fA-F]{4}$/.test(hex)) {
              throw malformed("invalid \\u escape");
            }
            const cp = Number.parseInt(hex, 16);
            out += String.fromCharCode(cp);
            this.i += 4;
            break;
          }
          default:
            throw malformed("invalid escape");
        }
        this.i += 1;
      } else {
        // Copy one character (a JS string char; surrogate pairs copy as two units — byte-equivalent
        // to Rust copying a UTF-8 char, since the renderer re-escapes by code point identically).
        out += c;
        this.i += 1;
      }
    }
  }

  // Parse a `[…]` subscript list (the opening `[` consumed, not the wildcard form). Each subscript
  // is `index` or `index to index`; `index` is a number or `last`. Anything else → `0A000`.
  private parseSubscripts(): Subscript[] {
    const subs: Subscript[] = [];
    for (;;) {
      this.skipWs();
      const a = this.parseIndex();
      this.skipWs();
      let sub: Subscript;
      if (this.eatKeyword("to")) {
        this.skipWs();
        const b = this.parseIndex();
        this.skipWs();
        sub = { kind: "slice", from: a, to: b };
      } else {
        sub = { kind: "index", index: a };
      }
      subs.push(sub);
      const c = this.peek();
      if (c === ",") {
        this.i += 1;
        continue;
      }
      if (c === "]") {
        this.i += 1;
        return subs;
      }
      throw malformed("expected `,` or `]` in subscript");
    }
  }

  private parseIndex(): Index {
    if (this.eatKeyword("last")) {
      return { kind: "last" };
    }
    const c = this.peek();
    // A truncated path (no index where one is required) is malformed.
    if (c === undefined) {
      throw malformed("expected a subscript index");
    }
    // A non-numeric token starts an expression subscript (`$.a`, arithmetic) — a P1b follow-on.
    if (!(isAsciiDigit(c) || c === "-")) {
      throw unsupported("non-literal subscript expressions");
    }
    const start = this.i;
    if (this.peek() === "-") {
      this.i += 1;
    }
    while (this.i < this.s.length && isAsciiDigit(this.s[this.i]!)) {
      this.i += 1;
    }
    if (this.i === start + 1 && this.s[start] === "-") {
      throw malformed("expected digits after `-`");
    }
    const text = this.s.slice(start, this.i);
    const n = Number.parseInt(text, 10);
    if (!Number.isSafeInteger(n)) {
      throw malformed("subscript out of range");
    }
    return { kind: "number", value: n };
  }
}

// compile compiles a jsonpath source string (P1a structural subset). Malformed → `42601`; a
// valid-PG but unsupported construct → `0A000`.
export function compile(src: string): JsonPath {
  return new Parser(src).compile();
}

// ---------------------------------------------------------------------------------------------
// Evaluation (jsonpath.md §3-4) — the lax/strict ordered jsonb-item sequence (P1b structural subset).
// ---------------------------------------------------------------------------------------------

// eval evaluates a compiled path over a jsonb context item → the ordered SQL/JSON sequence
// (jsonpath.md §3). Each accessor is a `seq → seq` map applied left to right. `lax` (default)
// auto-unwraps arrays (§4.1) and suppresses structural navigation failures (§4.2); `strict` raises.
// The P1b structural subset (no filters / item methods / arithmetic — those are still `0A000` at
// compile). Port of impl/rust/src/jsonpath.rs `eval`.
export function evalPath(path: JsonPath, ctx: JsonNode): JsonNode[] {
  let seq: JsonNode[] = [ctx];
  for (const step of path.steps) {
    const next: JsonNode[] = [];
    for (const item of seq) {
      applyStep(step, item, path.strict, next);
    }
    seq = next;
  }
  return seq;
}

function applyStep(step: Step, item: JsonNode, strict: boolean, out: JsonNode[]): void {
  switch (step.kind) {
    case "member": {
      // lax: a member accessor on an array unwraps it ONE level first (§4.1.1).
      if (!strict && item.kind === "array") {
        for (const e of item.elements) {
          memberAccess(e, step.key, strict, out);
        }
        return;
      }
      memberAccess(item, step.key, strict, out);
      return;
    }
    case "wildcardMember": {
      if (!strict && item.kind === "array") {
        for (const e of item.elements) {
          wildcardMember(e, strict, out);
        }
        return;
      }
      wildcardMember(item, strict, out);
      return;
    }
    case "subscripts": {
      // [i] on a non-array: lax treats the item as a singleton array (§4.1.2); strict raises.
      let elems: JsonNode[];
      if (item.kind === "array") {
        elems = item.elements;
      } else if (!strict) {
        elems = [item];
      } else {
        throw engineError(
          "invalid_sql_json_subscript",
          "jsonpath array accessor can only be applied to an array",
        );
      }
      for (const sub of step.subs) {
        subscript(elems, sub, strict, out);
      }
      return;
    }
    case "wildcardElement": {
      // [*] on a non-array: lax → the singleton item; strict raises.
      if (item.kind === "array") {
        for (const e of item.elements) {
          out.push(e);
        }
        return;
      }
      if (!strict) {
        out.push(item);
        return;
      }
      throw engineError(
        "invalid_sql_json_subscript",
        "jsonpath wildcard array accessor can only be applied to an array",
      );
    }
  }
}

function memberAccess(item: JsonNode, key: string, strict: boolean, out: JsonNode[]): void {
  if (item.kind === "object") {
    const member = item.members.find((m) => m.key === key);
    if (member !== undefined) {
      out.push(member.value);
    } else if (strict) {
      throw engineError(
        "sql_json_item_cannot_be_cast_to_target_type",
        `JSON object does not contain key "${key}"`,
      );
    }
    // lax: a missing member contributes no item (§4.2 rule 5).
    return;
  }
  if (strict) {
    throw engineError(
      "sql_json_object_not_found",
      "jsonpath member accessor can only be applied to an object",
    );
  }
  // lax: a member accessor on a non-object/non-array contributes no item.
}

function wildcardMember(item: JsonNode, strict: boolean, out: JsonNode[]): void {
  if (item.kind === "object") {
    for (const m of item.members) {
      out.push(m.value);
    }
    return;
  }
  if (strict) {
    throw engineError(
      "sql_json_object_not_found",
      "jsonpath wildcard member accessor can only be applied to an object",
    );
  }
}

function resolveIndex(i: Index, len: number): number {
  return i.kind === "number" ? i.value : len - 1;
}

function subscript(elems: JsonNode[], sub: Subscript, strict: boolean, out: JsonNode[]): void {
  const len = elems.length;
  if (sub.kind === "index") {
    const i = resolveIndex(sub.index, len);
    if (i >= 0 && i < len) {
      out.push(elems[i]!);
    } else if (strict) {
      throw engineError("invalid_sql_json_subscript", "jsonpath array subscript is out of bounds");
    }
    // lax: an out-of-range subscript contributes no item.
    return;
  }
  // A slice `i to j` → the clamped inclusive range (no error).
  const from = Math.max(resolveIndex(sub.from, len), 0);
  const to = Math.min(resolveIndex(sub.to, len), len - 1);
  for (let i = from; i <= to; i++) {
    out.push(elems[i]!);
  }
}

function writeIndex(i: Index): string {
  return i.kind === "number" ? i.value.toString() : "last";
}

// writeQuoted renders a member key as a canonical jsonpath quoted string (`"…"` with JSON
// escaping). Matches PostgreSQL's escaping and the Rust write_quoted.
function writeQuoted(k: string): string {
  let out = '"';
  for (const c of k) {
    const code = c.codePointAt(0)!;
    if (c === '"') {
      out += '\\"';
    } else if (c === "\\") {
      out += "\\\\";
    } else if (c === "\n") {
      out += "\\n";
    } else if (c === "\r") {
      out += "\\r";
    } else if (c === "\t") {
      out += "\\t";
    } else if (code < 0x20) {
      out += "\\u" + code.toString(16).padStart(4, "0");
    } else {
      out += c;
    }
  }
  out += '"';
  return out;
}

// render is the canonical render (spec/design/jsonpath.md §2): `strict` kept / `lax` omitted; member
// keys quoted; `[*]`, `[i]`, `[i to j]` subscripts; matches PostgreSQL's `jsonpath_out`.
export function render(jp: JsonPath): string {
  let out = "";
  if (jp.strict) {
    out += "strict ";
  }
  out += "$";
  for (const step of jp.steps) {
    switch (step.kind) {
      case "member":
        out += ".";
        out += writeQuoted(step.key);
        break;
      case "wildcardMember":
        out += ".*";
        break;
      case "wildcardElement":
        out += "[*]";
        break;
      case "subscripts": {
        out += "[";
        for (let n = 0; n < step.subs.length; n++) {
          if (n > 0) {
            out += ",";
          }
          const s = step.subs[n]!;
          if (s.kind === "index") {
            out += writeIndex(s.index);
          } else {
            out += writeIndex(s.from);
            out += " to ";
            out += writeIndex(s.to);
          }
        }
        out += "]";
        break;
      }
    }
  }
  return out;
}
