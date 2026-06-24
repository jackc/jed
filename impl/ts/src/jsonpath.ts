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
import { jsonbIn, jsonCompactOut, jsonNodeCmp, type JsonNode } from "./json.ts";

// A subscript index: a non-negative integer literal or the `last` sentinel.
export type Index = { kind: "number"; value: number } | { kind: "last" };

// One subscript: a single index or an `i to j` slice.
export type Subscript = { kind: "index"; index: Index } | { kind: "slice"; from: Index; to: Index };

// One accessor step.
export type Step =
  | { kind: "member"; key: string } // `.key` — a member accessor (the key, unescaped).
  | { kind: "wildcardMember" } // `.*` — the wildcard member accessor.
  | { kind: "subscripts"; subs: Subscript[] } // `[s, …]` — one or more subscripts.
  | { kind: "wildcardElement" } // `[*]` — the wildcard element accessor.
  | { kind: "filter"; pred: Pred }; // `?(predicate)` — a filter (§4).

// A filter predicate (jsonpath.md §4, the P1b comparison subset). 3-valued — `not`/`and`/`or`
// follow SQL/JSON's Kleene logic, but a filter keeps an item only when the predicate is definitely
// TRUE.
export type Pred =
  | { kind: "or"; a: Pred; b: Pred }
  | { kind: "and"; a: Pred; b: Pred }
  | { kind: "not"; p: Pred }
  // `lhs cmp rhs` — an existential comparison (true if SOME pair of items compares true).
  | { kind: "compare"; lhs: FiltExpr; op: CmpOp; rhs: FiltExpr };

// A comparison operand inside a filter: a `@`/`$`-rooted accessor path, or a scalar literal.
export type FiltExpr =
  // `@`-rooted (`fromRoot = false`) or `$`-rooted (`true`) accessor path.
  | { kind: "path"; fromRoot: boolean; steps: Step[] }
  // A scalar literal — a JSON number / string / boolean / null.
  | { kind: "lit"; node: JsonNode };

// A jsonpath comparison operator (`==`, `!=`/`<>`, `<`, `<=`, `>`, `>=`).
export type CmpOp = "eq" | "ne" | "lt" | "le" | "gt" | "ge";

// A jsonpath body: an accessor path (produces a sequence) or a top-level boolean predicate
// (`$.a == 1`, for `jsonb_path_match` / `@@`; jsonpath.md §6). Port of the Rust `PathBody` enum.
export type PathBody =
  // An accessor path → an ordered jsonb-item sequence.
  | { kind: "path"; steps: Step[] }
  // A top-level predicate → a single boolean item (TRUE iff the predicate is definitely true).
  | { kind: "predicate"; pred: Pred };

// A compiled jsonpath: a mode flag + a body that is EITHER an accessor path OR a top-level predicate.
export type JsonPath = { strict: boolean; body: PathBody };

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
    // A parenthesized top-level predicate — `($.a == 1)`, which is also the canonical render of a
    // top-level predicate, so this round-trips render → compile.
    if (this.peek() === "(") {
      const pred = this.parsePred();
      this.skipWs();
      if (this.peek() !== undefined) {
        throw malformed("unexpected trailing input in predicate");
      }
      return { strict, body: { kind: "predicate", pred } };
    }
    // Remember the body start: if the accessor path turns out to be the LHS of a TOP-LEVEL
    // predicate (`$.a == 1`, for jsonb_path_match / @@), we re-parse from here as a predicate.
    const bodyStart = this.i;
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
    const steps = this.parseSteps();
    this.skipWs();
    // After the accessor path: a comparison / logical operator makes the whole thing a top-level
    // predicate (re-parse from the body start as a predicate); arithmetic is a P1b follow-on.
    const trailing = this.peek();
    if (trailing === undefined) {
      // (an empty `steps` is `$` alone — the valid root document.)
      return { strict, body: { kind: "path", steps } };
    }
    if (
      trailing === "=" ||
      trailing === "<" ||
      trailing === ">" ||
      trailing === "!" ||
      trailing === "&" ||
      trailing === "|"
    ) {
      this.i = bodyStart;
      const pred = this.parsePred();
      this.skipWs();
      if (this.peek() !== undefined) {
        throw malformed("unexpected trailing input in predicate");
      }
      return { strict, body: { kind: "predicate", pred } };
    }
    if (
      trailing === "+" ||
      trailing === "-" ||
      trailing === "*" ||
      trailing === "/" ||
      trailing === "%"
    ) {
      throw unsupported("path arithmetic");
    }
    // A trailing WORD predicate operator (`like_regex`, `starts with`, `is unknown`) is a top-level
    // predicate too — deferred `0A000` (not malformed). Any other word is malformed.
    if ((trailing >= "a" && trailing <= "z") || (trailing >= "A" && trailing <= "Z")) {
      const rest = this.s.slice(this.i);
      if (rest.startsWith("like_regex") || rest.startsWith("starts") || rest.startsWith("is")) {
        throw unsupported("top-level predicate expressions");
      }
      throw malformed("unexpected trailing input in path");
    }
    throw malformed("unexpected trailing input in path");
  }

  // Parse a sequence of accessor steps (`.key`, `.*`, `[subscripts]`, `[*]`, `?(filter)`), stopping
  // at the first non-accessor character (EOF, a comparison/logical operator, `)`, etc).
  private parseSteps(): Step[] {
    const steps: Step[] = [];
    for (;;) {
      this.skipWs();
      const c = this.peek();
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
        this.i += 1;
        this.skipWs();
        if (!this.eat("(")) {
          throw malformed("expected `(` after `?`");
        }
        const pred = this.parsePred();
        this.skipWs();
        if (!this.eat(")")) {
          throw malformed("expected `)` after a filter predicate");
        }
        steps.push({ kind: "filter", pred });
      } else {
        break;
      }
    }
    return steps;
  }

  // Parse a filter predicate (P1b comparison subset): `||` over `&&` over `!` / `(…)` / comparison.
  private parsePred(): Pred {
    let left = this.parseAnd();
    for (;;) {
      this.skipWs();
      if (this.eatOp("||")) {
        const right = this.parseAnd();
        left = { kind: "or", a: left, b: right };
      } else {
        return left;
      }
    }
  }

  private parseAnd(): Pred {
    let left = this.parseNot();
    for (;;) {
      this.skipWs();
      if (this.eatOp("&&")) {
        const right = this.parseNot();
        left = { kind: "and", a: left, b: right };
      } else {
        return left;
      }
    }
  }

  private parseNot(): Pred {
    this.skipWs();
    if (this.eat("!")) {
      this.skipWs();
      if (!this.eat("(")) {
        throw malformed("expected `(` after `!`");
      }
      const inner = this.parsePred();
      this.skipWs();
      if (!this.eat(")")) {
        throw malformed("expected `)` after `!(`");
      }
      return { kind: "not", p: inner };
    }
    if (this.peek() === "(") {
      this.i += 1;
      const inner = this.parsePred();
      this.skipWs();
      if (!this.eat(")")) {
        throw malformed("expected `)` in predicate");
      }
      return inner;
    }
    return this.parseComparison();
  }

  // `filter_expr cmp filter_expr` — the only leaf predicate this slice (`exists` / `like_regex` /
  // `starts with` / `is unknown` are a follow-on).
  private parseComparison(): Pred {
    const lhs = this.parseFilterExpr();
    this.skipWs();
    let op: CmpOp;
    if (this.eatOp("==")) {
      op = "eq";
    } else if (this.eatOp("!=") || this.eatOp("<>")) {
      op = "ne";
    } else if (this.eatOp("<=")) {
      op = "le";
    } else if (this.eatOp(">=")) {
      op = "ge";
    } else if (this.eat("<")) {
      op = "lt";
    } else if (this.eat(">")) {
      op = "gt";
    } else {
      throw unsupported(
        "filter predicates other than a comparison (exists / like_regex / starts with)",
      );
    }
    const rhs = this.parseFilterExpr();
    return { kind: "compare", lhs, op, rhs };
  }

  // A comparison operand: a `@`/`$`-rooted accessor path, or a scalar literal.
  private parseFilterExpr(): FiltExpr {
    this.skipWs();
    const c = this.peek();
    if (c === "@") {
      this.i += 1;
      return { kind: "path", fromRoot: false, steps: this.parseSteps() };
    }
    if (c === "$") {
      this.i += 1;
      const next = this.peek();
      if (next !== undefined && (isMemberStart(next) || next === '"')) {
        throw unsupported("path variables `$name`");
      }
      return { kind: "path", fromRoot: true, steps: this.parseSteps() };
    }
    if (c === '"') {
      return { kind: "lit", node: { kind: "string", value: this.parseQuoted() } };
    }
    if (c !== undefined && (isAsciiDigit(c) || c === "-")) {
      return { kind: "lit", node: this.parseNumber() };
    }
    if (this.eatKeyword("true")) {
      return { kind: "lit", node: { kind: "bool", value: true } };
    }
    if (this.eatKeyword("false")) {
      return { kind: "lit", node: { kind: "bool", value: false } };
    }
    if (this.eatKeyword("null")) {
      return { kind: "lit", node: { kind: "null" } };
    }
    throw malformed("expected a comparison operand");
  }

  // Parse a JSON number literal in a filter (integer or decimal) → a `number` node. Reuses the json
  // number parser (a bare number is valid JSON).
  private parseNumber(): JsonNode {
    const start = this.i;
    if (this.peek() === "-") {
      this.i += 1;
    }
    for (;;) {
      const ch = this.peek();
      if (
        ch !== undefined &&
        (isAsciiDigit(ch) || ch === "." || ch === "e" || ch === "E" || ch === "+" || ch === "-")
      ) {
        this.i += 1;
      } else {
        break;
      }
    }
    const text = this.s.slice(start, this.i);
    let node: JsonNode;
    try {
      node = jsonbIn(text);
    } catch {
      throw malformed("invalid number literal");
    }
    if (node.kind !== "number") {
      throw malformed("invalid number literal");
    }
    return node;
  }

  // Consume a multi-character operator token if it appears at the cursor.
  private eatOp(op: string): boolean {
    if (this.s.startsWith(op, this.i)) {
      this.i += op.length;
      return true;
    }
    return false;
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
  if (path.body.kind === "path") {
    return evalSteps(path.body.steps, ctx, ctx, path.strict);
  }
  // A top-level predicate → a single boolean item: TRUE iff the predicate is definitely true
  // (unknown / false both render as `false`, matching PG's jsonb_path_query).
  const truth = evalPred(path.body.pred, ctx, ctx, path.strict) === true;
  return [{ kind: "bool", value: truth }];
}

// evalSteps evaluates an accessor-step sequence over a seed item, with `root` as the document `$`
// (for a filter's `$`-rooted operand). Port of impl/rust/src/jsonpath.rs `eval_steps`.
function evalSteps(steps: Step[], seed: JsonNode, root: JsonNode, strict: boolean): JsonNode[] {
  let seq: JsonNode[] = [seed];
  for (const step of steps) {
    const next: JsonNode[] = [];
    for (const item of seq) {
      applyStep(step, item, strict, root, next);
    }
    seq = next;
  }
  return seq;
}

function applyStep(
  step: Step,
  item: JsonNode,
  strict: boolean,
  root: JsonNode,
  out: JsonNode[],
): void {
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
    case "filter": {
      // `?(predicate)` — keep the current item when the predicate is definitely TRUE (§4). The
      // predicate's `@` is the item, `$` is the document root.
      if (evalPred(step.pred, item, root, strict) === true) {
        out.push(item);
      }
      return;
    }
  }
}

// evalPred evaluates a filter predicate to a Kleene truth value (`true`/`false`/`null` = unknown).
// Port of impl/rust/src/jsonpath.rs `eval_pred` (the Rust `Option<bool>`: Some(true)/Some(false)/None
// → true/false/null here).
function evalPred(pred: Pred, current: JsonNode, root: JsonNode, strict: boolean): boolean | null {
  switch (pred.kind) {
    case "or": {
      const x = evalPred(pred.a, current, root, strict);
      const y = evalPred(pred.b, current, root, strict);
      if (x === true || y === true) return true;
      if (x === false && y === false) return false;
      return null;
    }
    case "and": {
      const x = evalPred(pred.a, current, root, strict);
      const y = evalPred(pred.b, current, root, strict);
      if (x === false || y === false) return false;
      if (x === true && y === true) return true;
      return null;
    }
    case "not": {
      const x = evalPred(pred.p, current, root, strict);
      return x === null ? null : !x;
    }
    case "compare":
      return evalCompare(pred.lhs, pred.op, pred.rhs, current, root, strict);
  }
}

// evalCompare is the existential comparison (§4): true if SOME pair `(a in lhs-seq, b in rhs-seq)`
// compares true. An empty operand or all-incomparable pairs → null (unknown); else false.
function evalCompare(
  lhs: FiltExpr,
  op: CmpOp,
  rhs: FiltExpr,
  current: JsonNode,
  root: JsonNode,
  strict: boolean,
): boolean | null {
  const ls = evalFiltExpr(lhs, current, root, strict);
  const rs = evalFiltExpr(rhs, current, root, strict);
  if (ls.length === 0 || rs.length === 0) {
    return null;
  }
  let anyUnknown = false;
  for (const a of ls) {
    for (const b of rs) {
      const r = compareNodes(a, op, b);
      if (r === true) return true;
      if (r === null) anyUnknown = true;
    }
  }
  return anyUnknown ? null : false;
}

// evalFiltExpr evaluates a filter operand to its jsonb-item sequence (a `@`/`$` path) or a singleton
// literal. A navigation error inside a filter operand → no items (the comparison is just unknown),
// never propagated (§4.2: filter operands never raise, even in strict).
function evalFiltExpr(e: FiltExpr, current: JsonNode, root: JsonNode, strict: boolean): JsonNode[] {
  if (e.kind === "lit") {
    return [e.node];
  }
  const seed = e.fromRoot ? root : current;
  try {
    return evalSteps(e.steps, seed, root, strict);
  } catch {
    return [];
  }
}

// compareNodes compares two jsonb scalars under a jsonpath operator. Only same-type number/string
// compare by order; booleans / nulls compare only by `==`/`!=`; any other (mixed-type) pair is null
// (unknown). Number comparison reuses the json comparator (jsonNodeCmp); string is its UTF-8 byte
// order — both via jsonNodeCmp on same-type operands.
function compareNodes(a: JsonNode, op: CmpOp, b: JsonNode): boolean | null {
  // Same-type only; mixed types are not comparable.
  let ord: number;
  let orderOk: boolean;
  if (a.kind === "number" && b.kind === "number") {
    ord = jsonNodeCmp(a, b);
    orderOk = true;
  } else if (a.kind === "string" && b.kind === "string") {
    ord = jsonNodeCmp(a, b);
    orderOk = true;
  } else if (a.kind === "bool" && b.kind === "bool") {
    ord = jsonNodeCmp(a, b);
    orderOk = false; // booleans support only equality
  } else if (a.kind === "null" && b.kind === "null") {
    ord = 0;
    orderOk = false; // nulls support only equality
  } else {
    return null; // mixed types are not comparable
  }
  switch (op) {
    case "eq":
      return ord === 0;
    case "ne":
      return ord !== 0;
    case "lt":
      return orderOk ? ord < 0 : null;
    case "le":
      return orderOk ? ord <= 0 : null;
    case "gt":
      return orderOk ? ord > 0 : null;
    case "ge":
      return orderOk ? ord >= 0 : null;
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
// keys quoted; `[*]`, `[i]`, `[i to j]` subscripts; `?(predicate)` filters; matches PostgreSQL's
// `jsonpath_out`.
export function render(jp: JsonPath): string {
  let out = jp.strict ? "strict " : "";
  if (jp.body.kind === "path") {
    out += "$";
    out += writeSteps(jp.body.steps);
  } else {
    // A top-level predicate renders parenthesized (PG's `jsonpath_out`): `($."a" == 1)`.
    out += "(";
    out += writePred(jp.body.pred);
    out += ")";
  }
  return out;
}

// writeSteps renders an accessor-step sequence (shared by the path render and a filter's `@`/`$`
// operand). Port of impl/rust/src/jsonpath.rs `write_steps`.
function writeSteps(steps: Step[]): string {
  let out = "";
  for (const step of steps) {
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
      case "filter":
        out += "?(";
        out += writePred(step.pred);
        out += ")";
        break;
    }
  }
  return out;
}

// writePred renders a filter predicate (PG's `?(…)` form: `&&`/`||` spaced, `!(…)`, `a op b`
// spaced). Port of impl/rust/src/jsonpath.rs `write_pred`.
function writePred(pred: Pred): string {
  switch (pred.kind) {
    case "or":
      return writePred(pred.a) + " || " + writePred(pred.b);
    case "and":
      return writePred(pred.a) + " && " + writePred(pred.b);
    case "not":
      return "!(" + writePred(pred.p) + ")";
    case "compare":
      return writeFiltExpr(pred.lhs) + " " + cmpOpSymbol(pred.op) + " " + writeFiltExpr(pred.rhs);
  }
}

function cmpOpSymbol(op: CmpOp): string {
  switch (op) {
    case "eq":
      return "==";
    case "ne":
      return "!=";
    case "lt":
      return "<";
    case "le":
      return "<=";
    case "gt":
      return ">";
    case "ge":
      return ">=";
  }
}

function writeFiltExpr(e: FiltExpr): string {
  if (e.kind === "lit") {
    return jsonCompactOut(e.node);
  }
  return (e.fromRoot ? "$" : "@") + writeSteps(e.steps);
}
