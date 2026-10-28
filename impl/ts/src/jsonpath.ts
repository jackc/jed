// The `jsonpath` type's compiler, canonical renderer, and evaluator (spec/design/jsonpath.md).
//
// The compiled surface: lax/strict mode; a primary (`$`, `@` inside a filter, a scalar literal,
// `last` inside a subscript, or a parenthesized expression or predicate) followed by accessor steps
// (`.key`, `.*`, `.**`, `[subscripts]` of any expression, `[*]`, `to` slices, `?(filter)`, and the
// item methods of §3.1); arithmetic (`+ - * / %`, unary `+ -`, §3.2); a top-level predicate
// (comparison, `exists`, `like_regex`, `starts with`, `is unknown`, §4.4). Path variables (`$name`,
// `$"name"`) are bound by the caller's Vars. The datetime methods and the `like_regex` `x` flag are
// `0A000` (jsonpath.md §3.1, §4.3). A malformed path is `42601`. The compiled program is a pure
// function of the source — kept
// byte-identical cross-core by the conformance suite (CLAUDE.md §5: a hand-written parser, never
// codegenned).
//
// This is a port of impl/rust/src/jsonpath.rs. The Rust parser walks bytes; this one walks JS string
// characters, which is equivalent for the ASCII grammar (keywords, digits, punctuation); quoted
// strings copy their characters verbatim. Byte lengths (the `.keyvalue()` layout) use utf8Length.

import {
  Decimal,
  EXP_LIMIT,
  MAX_PRECISION,
  decimalFromParts,
  workDiv,
  workLinear,
  workMod,
  workMul,
} from "./decimal.ts";
import { EngineError, engineError } from "./errors.ts";
import type { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import { utf8Length } from "./memsize.ts";
import { foldLowerSimple, type PropertyTable } from "./collation.ts";
import {
  type RegexProgram,
  compileRegexMode,
  foldRegexPattern,
  quoteRegex,
  regexIsMatch,
  regexNinst,
} from "./regex.ts";
import { compareTextC } from "./value.ts";
import {
  HIGH_SURROGATE_DETAIL,
  LOW_SURROGATE_DETAIL,
  MAX_JSON_DEPTH,
  escapeSyntaxError,
  jsonCompactOut,
  jsonNodeCmp,
  type JsonNode,
  keyCmp,
  typeofName,
  unicodeZeroError,
} from "./json.ts";
import { parseDecimalLiteral, parseFloatLiteral, parseIntLiteral, trimLit } from "./kernels.ts";

// A subscript index: a number literal (as written — evaluation truncates it and rejects one outside
// the integer range), the `last` sentinel, or any other expression (§3.2).
export type Index =
  | { kind: "number"; value: Decimal }
  | { kind: "last" }
  | { kind: "expr"; expr: PathExpr };

// One subscript: a single index or an `i to j` slice.
export type Subscript = { kind: "index"; index: Index } | { kind: "slice"; from: Index; to: Index };

// An item method (jsonpath.md §3.1). The datetime family is not supported (`0A000`), by decision.
export type MethodName =
  | "type"
  | "size"
  | "double"
  | "ceiling"
  | "floor"
  | "abs"
  | "keyvalue"
  | "bigint"
  | "boolean"
  | "integer"
  | "number"
  | "string"
  | "decimal";

// An item method; `precision`/`scale` hold `.decimal(p [, s])`'s optional integer arguments.
export type Method = { name: MethodName; precision: Decimal | null; scale: Decimal | null };

const METHOD_NAMES: readonly MethodName[] = [
  "type",
  "size",
  "double",
  "ceiling",
  "floor",
  "abs",
  "keyvalue",
  "bigint",
  "boolean",
  "integer",
  "number",
  "string",
];

// One accessor step.
export type Step =
  | { kind: "member"; key: string } // `.key` — a member accessor (the key, unescaped).
  | { kind: "wildcardMember" } // `.*` — the wildcard member accessor.
  | { kind: "subscripts"; subs: Subscript[] } // `[s, …]` — one or more subscripts.
  | { kind: "wildcardElement" } // `[*]` — the wildcard element accessor.
  | { kind: "filter"; pred: Pred } // `?(predicate)` — a filter (§4).
  | { kind: "method"; method: Method } // `.name(…)` — an item method (§3.1).
  // `.**{first to last}` — the item and its descendants within the level bounds (§3.3); a bound of
  // `last` is LEVEL_LAST.
  | { kind: "any"; first: number; last: number };

// LEVEL_LAST is a `.**` level bound of `last`: PostgreSQL stores it as the largest uint32.
const LEVEL_LAST = 4294967295;

// Where a path starts: `$` (the context item), `@` (the item a filter is testing), a scalar literal,
// `last` (the innermost subscripted array's last index — subscript expressions only), unary
// arithmetic over each item of an operand, binary arithmetic over two singleton numbers (§3.2), or
// `(predicate)` followed by accessors (its truth value as an item: true, false, or null when unknown).
export type Primary =
  | { kind: "root" }
  | { kind: "current" }
  | { kind: "lit"; node: JsonNode }
  | { kind: "last" }
  | { kind: "var"; name: string } // `$name` / `$"name"` — a path variable, bound by the caller's Vars (§5).
  | { kind: "unary"; op: UnaryOp; arg: PathExpr }
  | { kind: "binary"; op: ArithOp; l: PathExpr; r: PathExpr }
  | { kind: "pred"; pred: Pred };

// A unary arithmetic operator.
export type UnaryOp = "+" | "-";

// A binary arithmetic operator, as written and as PostgreSQL names it in an error message.
export type ArithOp = "+" | "-" | "*" | "/" | "%";

// arithPriority is PostgreSQL's operationPriority: `+ -` bind looser than `* / %`.
function arithPriority(op: ArithOp): number {
  return op === "+" || op === "-" ? 3 : 4;
}

// A primary followed by accessor steps — the top-level path body, every comparison operand, and
// every arithmetic operand. A literal primary with no steps is a plain literal operand.
export type PathExpr = { primary: Primary; steps: Step[] };

// A parsed operand: an expression or a predicate — PostgreSQL's expr_or_predicate. A parenthesized
// predicate is an expression only when accessors follow it. parenPred marks a predicate that is
// exactly `( predicate )`, the only operand of `is unknown`.
type Parsed = { kind: "expr"; expr: PathExpr } | { kind: "pred"; pred: Pred; parenPred?: boolean };

// needExpr requires an expression where the grammar takes one (42601 for a bare predicate).
function needExpr(p: Parsed): PathExpr {
  if (p.kind !== "expr") throw malformed("expected an expression, not a predicate");
  return p.expr;
}

// needPred requires a predicate where the grammar takes one (42601 for an expression).
function needPred(p: Parsed): Pred {
  if (p.kind !== "pred") throw malformed("expected a predicate");
  return p.pred;
}

// A filter predicate (jsonpath.md §4, the comparison subset). 3-valued — `not`/`and`/`or` follow
// SQL/JSON's Kleene logic, but a filter keeps an item only when the predicate is definitely TRUE.
export type Pred =
  | { kind: "or"; a: Pred; b: Pred }
  | { kind: "and"; a: Pred; b: Pred }
  | { kind: "not"; p: Pred }
  // `lhs cmp rhs` — an existential comparison (true if SOME pair of items compares true).
  | { kind: "compare"; lhs: PathExpr; op: CmpOp; rhs: PathExpr }
  // `exists (e)` — e's sequence is non-empty.
  | { kind: "exists"; e: PathExpr }
  // `(p) is unknown`.
  | { kind: "isUnknown"; p: Pred }
  // `lhs starts with rhs` — rhs a string literal or a variable.
  | { kind: "startsWith"; lhs: PathExpr; rhs: PathExpr }
  // `lhs like_regex "pattern" flag "flags"` — the pattern as written, its flag bits, and its program.
  | { kind: "likeRegex"; lhs: PathExpr; pattern: string; flags: number; regex: RegexProgram };

// like_regex flag bits, in PostgreSQL's print order (jsonpath.md §2.3).
const REGEX_I = 1;
const REGEX_S = 2;
const REGEX_M = 4;
const REGEX_X = 8;
const REGEX_Q = 16;

// REGEX_FLAG_CHARS is the flag characters in bit order.
const REGEX_FLAG_CHARS = "ismxq";

// A jsonpath comparison operator (`==`, `!=`/`<>`, `<`, `<=`, `>`, `>=`).
export type CmpOp = "eq" | "ne" | "lt" | "le" | "gt" | "ge";

// A jsonpath body: an accessor path (produces a sequence) or a top-level boolean predicate
// (`$.a == 1`, for `jsonb_path_match` / `@@`; jsonpath.md §6). Port of the Rust `PathBody` enum.
export type PathBody =
  // An accessor path → an ordered jsonb-item sequence.
  | { kind: "path"; expr: PathExpr }
  // A top-level predicate → a single boolean item (or JSON null when unknown).
  | { kind: "predicate"; pred: Pred };

// A compiled jsonpath: a mode flag + a body that is EITHER an accessor path OR a top-level predicate.
// regexInsts is the instruction count of every like_regex program in the path, which a metered
// compile charges as regex_compile (jsonpath.md §7).
// prop is the casing table (collation.md §16) the path was compiled under — the handle environment's,
// undefined ⇒ ASCII — which an `i`-flagged like_regex folds its pattern and subject with.
export type JsonPath = {
  strict: boolean;
  body: PathBody;
  regexInsts: number;
  prop: PropertyTable | undefined;
};

// A jsonpath construct that is valid in PostgreSQL but not yet supported by jed (a deferred
// follow-on): `0A000`, a documented divergence.
function unsupported(what: string): EngineError {
  return engineError("feature_not_supported", `jsonpath ${what} is not supported yet`);
}

// A malformed jsonpath literal: `42601` (PostgreSQL's syntax-error class for a bad path literal).
// codePointText is a decoded escape's code point as text — PG's addUnicodeChar: NUL is 22P05 (text
// cannot hold it), a value above U+10FFFF 42601 ("invalid Unicode code point").
function codePointText(ch: number): string {
  if (ch === 0) throw unicodeZeroError();
  if (ch > 0x10ffff) throw engineError("syntax_error", "invalid Unicode code point");
  return String.fromCodePoint(ch);
}

function malformed(detail: string): EngineError {
  return engineError("syntax_error", `invalid jsonpath: ${detail}`);
}

// isSpecial is PostgreSQL's jsonpath scanner `special` set (jsonpath_scan.l): each is a token of its
// own, so it ends an unquoted name.
function isSpecial(c: string): boolean {
  return "?%$.[]{}()|&!=<>@#,*:-+/".includes(c);
}

// isOther is PostgreSQL's jsonpath `other` class: a character that may appear unescaped in an
// unquoted name — not special, whitespace, `\`, or `"`. Every non-ASCII character is one.
function isOther(c: string | undefined): boolean {
  return c !== undefined && !isSpecial(c) && !isJsonpathWhitespace(c) && c !== "\\" && c !== '"';
}

// pgNumberLen is the length of the longest PostgreSQL jsonpath number token at index `i` of `s`
// (where a digit starts), counting the rules a name competes with: an integer (decimal, or a
// `0x`/`0o`/`0b` prefix), a decimal, a real, an incomplete exponent (`1e+`), and each with one
// trailing `other` character (the "trailing junk" rules). `_` digit separators count, as PostgreSQL
// 16+ lexes them. A number token is ASCII and its junk rule adds one byte, so the code-unit length
// returned is PostgreSQL's byte length.
function pgNumberLen(s: string, i: number): number {
  const dec = (c: string | undefined): boolean => isAsciiDigit(c);
  const hex = (c: string | undefined): boolean => c !== undefined && /^[0-9a-fA-F]$/.test(c);
  const oct = (c: string | undefined): boolean => c !== undefined && c >= "0" && c <= "7";
  const bin = (c: string | undefined): boolean => c === "0" || c === "1";
  // The end of `(_?digit)*` from j (a first digit precedes it).
  const digitRun = (j: number, ok: (c: string | undefined) => boolean): number => {
    for (;;) {
      if (ok(s[j])) j += 1;
      else if (s[j] === "_" && ok(s[j + 1])) j += 2;
      else return j;
    }
  };
  // A "trailing junk" rule: the token plus one `other` character.
  const junk = (end: number): number => (isOther(s[end]) ? end + 1 : end);
  // decinteger: `0`, or a nonzero digit and (_?digit)*.
  const intEnd = s[i] === "0" ? i + 1 : digitRun(i + 1, dec);
  let best = junk(intEnd);
  // hexinteger / octinteger / bininteger: `0x`, `0o`, `0b` and one or more digits.
  if (s[i] === "0") {
    const p = s[i + 1];
    let ok: ((c: string | undefined) => boolean) | null = null;
    if (p === "x" || p === "X") ok = hex;
    else if (p === "o" || p === "O") ok = oct;
    else if (p === "b" || p === "B") ok = bin;
    if (ok?.(s[i + 2])) best = Math.max(best, digitRun(i + 3, ok));
  }
  // decimal: decinteger `.` decdigits?
  const mantissas = [intEnd];
  if (s[intEnd] === ".") {
    const end = dec(s[intEnd + 1]) ? digitRun(intEnd + 2, dec) : intEnd + 1;
    best = Math.max(best, junk(end));
    mantissas.push(end);
  }
  // real: (decinteger | decimal) [Ee] [-+]? decdigits; realfail: … [Ee] [-+].
  for (const m of mantissas) {
    if (s[m] !== "e" && s[m] !== "E") continue;
    let k = m + 1;
    const signed = s[k] === "-" || s[k] === "+";
    if (signed) k += 1;
    if (dec(s[k])) best = Math.max(best, junk(digitRun(k + 1, dec)));
    else if (signed) best = Math.max(best, k);
  }
  return best - i;
}

// utf8UnitBytes is the UTF-8 byte count a UTF-16 code unit contributes (each half of a surrogate
// pair, two of the pair's four bytes).
function utf8UnitBytes(u: number): number {
  if (u < 0x80) return 1;
  if (u < 0x800) return 2;
  if (u >= 0xd800 && u <= 0xdfff) return 2;
  return 3;
}

// asciiLower folds only ASCII letters (PostgreSQL's pg_strncasecmp).
function asciiLower(w: string): string {
  return w.replace(/[A-Z]/g, (c) => String.fromCharCode(c.charCodeAt(0) + 32));
}

// keywordIs reports whether PostgreSQL keyword `kw` matches the decoded word `w`: exactly for `null` /
// `true` / `false`, ASCII-case-insensitively for the rest (checkKeyword).
function keywordIs(w: string, kw: string): boolean {
  if (kw === "null" || kw === "true" || kw === "false") return w === kw;
  return asciiLower(w) === kw;
}

function isAsciiDigit(c: string | undefined): boolean {
  return c !== undefined && c >= "0" && c <= "9";
}

function isAsciiAlpha(c: string | undefined): boolean {
  return c !== undefined && ((c >= "a" && c <= "z") || (c >= "A" && c <= "Z"));
}

// isJsonpathWhitespace is PostgreSQL's jsonpath whitespace set: space, tab, LF, CR, FF.
function isJsonpathWhitespace(c: string): boolean {
  return c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\f";
}

// Parser walks the source string char by char (mirrors Rust's byte cursor — see the module note).
class Parser {
  private readonly s: string;
  private i: number;
  // depth: the program nesting depth (spec/design/json.md §6.4) — +1 per filter `?(`, parenthesized
  // group, `!`, `&&`/`||` chain step, arithmetic operator, and subscript list, bounded by
  // MAX_JSON_DEPTH → 54001. Bounding it at the compiler bounds every derived walk (render,
  // evaluate).
  private depth: number;
  // inFilter: how many filters enclose the cursor — `@` is valid only inside one.
  private inFilter: number;
  // inSubscript: how many subscripts enclose the cursor — `last` is valid only inside one.
  private inSubscript: number;
  // regexInsts: the instruction count of the like_regex programs compiled so far.
  private regexInsts: number;
  // prop: the casing table an `i`-flagged like_regex folds under (undefined ⇒ ASCII).
  private readonly prop: PropertyTable | undefined;

  constructor(src: string, prop: PropertyTable | undefined) {
    this.s = src;
    this.prop = prop;
    this.i = 0;
    this.depth = 0;
    this.inFilter = 0;
    this.inSubscript = 0;
    this.regexInsts = 0;
  }

  // deepen descends one program nesting level, enforcing MAX_JSON_DEPTH (json.md §6.4). The caller
  // restores it on the success path (an error aborts the whole compile).
  private deepen(): void {
    this.depth += 1;
    if (this.depth > MAX_JSON_DEPTH) {
      throw engineError(
        "statement_too_complex",
        `jsonpath nesting depth exceeds the maximum of ${MAX_JSON_DEPTH}`,
      );
    }
  }

  private peek(): string | undefined {
    return this.i < this.s.length ? this.s[this.i] : undefined;
  }

  private peekAt(k: number): string | undefined {
    return this.i + k < this.s.length ? this.s[this.i + k] : undefined;
  }

  private eat(c: string): boolean {
    if (this.peek() === c) {
      this.i += 1;
      return true;
    }
    return false;
  }

  // skipWs skips what PostgreSQL's jsonpath scanner discards between tokens (jsonpath.md §2.4):
  // whitespace ([ \t\n\r\f]), `/* … */` comments, and a name that runs straight into a comment —
  // the scanner's non-quoted-string state enters the comment state without returning the name, so
  // `$.a/*c*/b` is `$."b"`. The name is still lexed, so its escape error is raised.
  private skipWs(): void {
    for (;;) {
      while (this.i < this.s.length && isJsonpathWhitespace(this.s[this.i]!)) {
        this.i += 1;
      }
      if (this.s.startsWith("/*", this.i)) {
        this.skipComment();
        continue;
      }
      if (this.wordAhead()) {
        const start = this.i;
        this.scanWord();
        if (this.s.startsWith("/*", this.i)) {
          this.skipComment();
          continue;
        }
        this.i = start;
      }
      return;
    }
  }

  // skipComment skips the `/* … */` comment at the cursor. Comments do not nest; one without its
  // `*/` is `42601`.
  private skipComment(): void {
    const end = this.s.indexOf("*/", this.i + 2);
    if (end < 0) throw malformed("unexpected end of comment");
    this.i = end + 2;
  }

  // wordAhead: whether PostgreSQL's scanner would lex a name (an identifier or keyword) at the
  // cursor — a backslash escape or an `other` character. A digit starts a number instead, unless the
  // run of `other` characters there is longer than every number token (flex's longest match: `1abc`
  // is a name, `12`, `1a`, and `1e3` are not). Lengths are PostgreSQL's UTF-8 byte counts (`12é` is
  // four bytes, longer than the three-byte trailing-junk token `12` + one byte, so a name).
  private wordAhead(): boolean {
    const c = this.peek();
    if (c === undefined) return false;
    if (c === "\\") return true;
    if (isAsciiDigit(c)) {
      let run = 0;
      for (let j = this.i; isOther(this.s[j]); j += 1) run += utf8UnitBytes(this.s.charCodeAt(j));
      return run > pgNumberLen(this.s, this.i);
    }
    return isOther(c);
  }

  // scanWord scans the name at the cursor (wordAhead) — PostgreSQL's non-quoted string state: a run
  // of `other` characters and backslash escapes (decoded as in a quoted string, jsonpath.md §2.1),
  // ended by a special character, whitespace, `"`, or the end of input.
  private scanWord(): string {
    let out = "";
    for (;;) {
      const c = this.peek();
      if (c === "\\") {
        out += this.parseEscape();
      } else if (isOther(c)) {
        out += c;
        this.i += 1;
      } else {
        return out;
      }
    }
  }

  // Consume keyword `kw` if the name at the cursor is that keyword (keywordIs). A name that is not —
  // or whose escapes fail to decode — is left unconsumed.
  eatKeyword(kw: string): boolean {
    if (!this.wordAhead()) return false;
    const start = this.i;
    try {
      if (keywordIs(this.scanWord(), kw)) return true;
    } catch (e) {
      if (!(e instanceof EngineError)) throw e;
    }
    this.i = start;
    return false;
  }

  // Compile a jsonpath source string. Malformed → `42601`; a valid-PG but unsupported construct →
  // `0A000`.
  compile(): JsonPath {
    this.skipWs();
    // Nothing but whitespace and comments: PostgreSQL's grammar yields no path, `22P02`.
    if (this.peek() === undefined) {
      throw engineError(
        "invalid_text_representation",
        `invalid input syntax for type jsonpath: "${this.s}"`,
      );
    }
    // Optional mode word: `strict` / `lax` (default lax).
    let strict: boolean;
    if (this.eatKeyword("strict")) {
      strict = true;
    } else {
      this.eatKeyword("lax");
      strict = false;
    }
    // The body is an expression (an accessor path, arithmetic) or a top-level predicate
    // (`$.a == 1`, for jsonb_path_match / @@) — PostgreSQL's expr_or_predicate.
    const body = this.parseOr();
    this.skipWs();
    if (this.peek() !== undefined) throw malformed("unexpected trailing input");
    const regexInsts = this.regexInsts;
    const prop = this.prop;
    return body.kind === "expr"
      ? { strict, body: { kind: "path", expr: body.expr }, regexInsts, prop }
      : { strict, body: { kind: "predicate", pred: body.pred }, regexInsts, prop };
  }

  // parsePostfix parses a primary and its accessor steps (PostgreSQL's accessor_expr). A
  // parenthesized expression followed by accessors continues it (`($.a).b` is `$.a.b`,
  // `(1 + 2).type()`); a parenthesized predicate followed by accessors yields its truth value; without
  // accessors the parentheses only group.
  private parsePostfix(): Parsed {
    this.skipWs();
    if (this.eat("(")) {
      this.deepen();
      const inner = this.parseOr();
      this.depth -= 1;
      this.skipWs();
      if (!this.eat(")")) throw malformed("expected `)`");
      const steps = this.parseSteps();
      if (steps.length === 0) {
        return inner.kind === "pred" ? { kind: "pred", pred: inner.pred, parenPred: true } : inner;
      }
      if (inner.kind === "pred") {
        return { kind: "expr", expr: { primary: { kind: "pred", pred: inner.pred }, steps } };
      }
      inner.expr.steps.push(...steps);
      return inner;
    }
    const c = this.peek();
    const next = this.peekAt(1);
    let primary: Primary;
    if (c === undefined) {
      throw malformed("unexpected end of input");
    } else if (c === "$") {
      this.i += 1;
      // `$"name"` / `$name` — a path variable. PostgreSQL's unquoted variable name is a run of
      // `other` characters (no escapes); a quoted one takes the string escapes (§2.1).
      const after = this.peek();
      if (after === '"') {
        primary = { kind: "var", name: this.parseQuoted() };
      } else if (isOther(after)) {
        const start = this.i;
        while (isOther(this.peek())) this.i += 1;
        primary = { kind: "var", name: this.s.slice(start, this.i) };
      } else {
        primary = { kind: "root" };
      }
    } else if (c === "@") {
      if (this.inFilter === 0) {
        throw malformed("@ is not allowed in root expressions");
      }
      this.i += 1;
      primary = { kind: "current" };
    } else if (c === '"') {
      primary = { kind: "lit", node: { kind: "string", value: this.parseQuoted() } };
    } else if (isAsciiDigit(c) && this.wordAhead()) {
      // A digit-led name (`1abc`) is not an operand.
      this.scanWord();
      throw malformed("a name is not an operand");
    } else if (isAsciiDigit(c) || (c === "." && isAsciiDigit(next))) {
      primary = { kind: "lit", node: this.parseNumber() };
    } else if (this.wordAhead()) {
      // A name: one of the keywords that start an operand, or else not an operand at all.
      const word = this.scanWord();
      if (keywordIs(word, "true")) {
        primary = { kind: "lit", node: { kind: "bool", value: true } };
      } else if (keywordIs(word, "false")) {
        primary = { kind: "lit", node: { kind: "bool", value: false } };
      } else if (keywordIs(word, "null")) {
        primary = { kind: "lit", node: { kind: "null" } };
      } else if (keywordIs(word, "last")) {
        if (this.inSubscript === 0) throw malformed("LAST is allowed only in array subscripts");
        primary = { kind: "last" };
      } else if (keywordIs(word, "exists")) {
        // No accessors follow `exists (…)`: it is a predicate, not an expression.
        return { kind: "pred", pred: this.parseExists() };
      } else {
        throw malformed("a name is not an operand");
      }
    } else {
      throw malformed("unexpected token");
    }
    return { kind: "expr", expr: { primary, steps: this.parseSteps() } };
  }

  // Parse a number literal (PostgreSQL's jsonpath scanner: an integer, `1.5`, `1.`, `.5`, an
  // exponent) → a `number` node. A number running into a letter or digit is trailing junk.
  private parseNumber(): JsonNode {
    const junk = (): EngineError => malformed("trailing junk after numeric literal");
    const neg = this.eat("-");
    const intStart = this.i;
    while (isAsciiDigit(this.peek())) this.i += 1;
    const intPart = this.s.slice(intStart, this.i);
    if (intPart.length > 1 && intPart.startsWith("0")) throw junk();
    let frac = "";
    if (this.peek() === ".") {
      const nc = this.peekAt(1);
      if (isAsciiDigit(nc)) {
        this.i += 1;
        const start = this.i;
        while (isAsciiDigit(this.peek())) this.i += 1;
        frac = this.s.slice(start, this.i);
      } else if ((nc === "e" || nc === "E") && intPart !== "" && this.exponentAt(2)) {
        // `1.e3` — an empty fraction, then an exponent.
        this.i += 1;
      } else if (isAsciiAlpha(nc) || nc === "_") {
        throw junk();
      } else if (nc === "*" || nc === '"' || nc === "[" || nc === ".") {
        // A step follows the point.
      } else if (intPart !== "") {
        // `1.` — a number with an empty fraction.
        this.i += 1;
      }
    }
    let exp: number | null = null;
    if (this.peek() === "e" || this.peek() === "E") {
      this.i += 1;
      let eneg = false;
      if (this.eat("-")) {
        eneg = true;
      } else {
        this.eat("+");
      }
      const start = this.i;
      let v = 0;
      while (isAsciiDigit(this.peek())) {
        if (v < EXP_LIMIT) {
          v = Math.min(v * 10 + (this.s.charCodeAt(this.i) - 48), EXP_LIMIT);
        }
        this.i += 1;
      }
      if (this.i === start) throw junk();
      exp = eneg ? -v : v;
    }
    // PostgreSQL's "trailing junk" rules: a number followed by any `other` character (`1a`, `1~`).
    if (isOther(this.peek())) throw junk();
    if (intPart === "" && frac === "") throw malformed("invalid number literal");
    const [digits, scale] = decimalFromParts(intPart, frac, exp);
    return { kind: "number", dec: Decimal.fromDigitsScale(neg, digits, scale).checkCap() };
  }

  // exponentAt reports whether exponent digits (optionally signed) start k characters past the cursor.
  private exponentAt(k: number): boolean {
    let c = this.peekAt(k);
    if (c === "+" || c === "-") c = this.peekAt(k + 1);
    return isAsciiDigit(c);
  }

  // Parse a sequence of accessor steps (`.key`, `.*`, `[subscripts]`, `[*]`, `?(filter)`,
  // `.method(…)`), stopping at the first non-accessor character (EOF, a comparison/logical operator,
  // `)`, etc).
  private parseSteps(): Step[] {
    const steps: Step[] = [];
    for (;;) {
      this.skipWs();
      const c = this.peek();
      if (c === ".") {
        this.i += 1;
        // PostgreSQL lexes `.5` / `.1abc` as a number token, never `.` and a name.
        if (isAsciiDigit(this.peek())) throw malformed("expected a member name after `.`");
        this.skipWs();
        const next = this.peek();
        if (this.eat("*")) {
          steps.push(this.eat("*") ? this.parseAnyLevels() : { kind: "wildcardMember" });
        } else if (next === '"') {
          steps.push({ kind: "member", key: this.parseQuoted() });
        } else if (this.wordAhead()) {
          // An unquoted name (jsonpath.md §2.1); followed by `(`, an item method.
          const name = this.scanWord();
          const save = this.i;
          this.skipWs();
          if (this.peek() === "(") {
            steps.push({ kind: "method", method: this.parseMethod(name) });
          } else {
            this.i = save;
            steps.push({ kind: "member", key: name });
          }
        } else {
          throw malformed("expected a member name after `.`");
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
        this.deepen();
        this.inFilter += 1;
        const pred = needPred(this.parseOr());
        this.inFilter -= 1;
        this.depth -= 1;
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

  // parseAnyLevels parses the optional `{level}` / `{level to level}` after `.**` (jsonpath.md §2).
  private parseAnyLevels(): Step {
    const save = this.i;
    this.skipWs();
    if (!this.eat("{")) {
      this.i = save;
      return { kind: "any", first: 0, last: LEVEL_LAST };
    }
    const first = this.parseAnyLevel();
    let last = first;
    this.skipWs();
    if (this.eatKeyword("to")) {
      last = this.parseAnyLevel();
      this.skipWs();
    }
    if (!this.eat("}")) throw malformed("expected `}` after a .** level");
    return { kind: "any", first, last };
  }

  // parseAnyLevel parses one `.**` level: an unsigned integer literal (PostgreSQL's INT_P, at most
  // 2147483647, else 22003) or `last`.
  private parseAnyLevel(): number {
    this.skipWs();
    if (this.eatKeyword("last")) return LEVEL_LAST;
    const start = this.i;
    while (isAsciiDigit(this.peek())) this.i += 1;
    const digits = this.s.slice(start, this.i);
    if (digits === "") throw malformed("expected a .** level");
    // A number running into a name character, a leading zero, or a fraction is not an integer.
    const c = this.peek();
    if (c === "." || isOther(c)) throw malformed("trailing junk after numeric literal");
    if (digits.length > 1 && digits.startsWith("0")) {
      throw malformed("trailing junk after numeric literal");
    }
    const v = BigInt(digits);
    if (v > 2147483647n) {
      throw engineError(
        "numeric_value_out_of_range",
        `value "${digits}" is out of range for type integer`,
      );
    }
    return Number(v);
  }

  // Parse an item method's argument list (the cursor is at its `(`). Method names are matched
  // case-insensitively, like PostgreSQL's keywords.
  private parseMethod(name: string): Method {
    this.i += 1; // (
    this.skipWs();
    const lower = name.toLowerCase();
    switch (lower) {
      case "datetime":
      case "date":
      case "time":
      case "time_tz":
      case "timestamp":
      case "timestamp_tz":
        throw unsupported(`item method .${lower}()`);
      case "decimal": {
        const args: Decimal[] = [];
        if (!this.eat(")")) {
          for (;;) {
            args.push(this.parseIntArg());
            this.skipWs();
            if (this.eat(")")) break;
            if (!this.eat(",")) {
              throw malformed("expected `,` or `)` in .decimal() arguments");
            }
          }
        }
        if (args.length > 2) {
          throw malformed(".decimal() can only have an optional precision[,scale]");
        }
        return { name: "decimal", precision: args[0] ?? null, scale: args[1] ?? null };
      }
    }
    const found = METHOD_NAMES.find((n) => n === lower);
    if (found === undefined) {
      throw malformed(`unknown item method .${name}()`);
    }
    if (!this.eat(")")) {
      throw malformed(`item method .${found}() takes no arguments`);
    }
    return { name: found, precision: null, scale: null };
  }

  // Parse a `.decimal()` argument: an optionally signed integer literal.
  private parseIntArg(): Decimal {
    this.skipWs();
    let neg = false;
    if (this.eat("-")) {
      neg = true;
    } else {
      this.eat("+");
    }
    this.skipWs();
    const start = this.i;
    while (isAsciiDigit(this.peek())) this.i += 1;
    const digits = this.s.slice(start, this.i);
    const after = this.peek();
    if (
      digits === "" ||
      (digits.length > 1 && digits.startsWith("0")) ||
      isOther(after) ||
      after === "."
    ) {
      throw malformed("expected an integer .decimal() argument");
    }
    return Decimal.fromDigitsScale(neg, digits, 0);
  }

  // parseOr parses an expression or predicate: `||` over `&&` over `!` / comparison over arithmetic
  // (PostgreSQL's jsonpath_gram.y precedence: `||`, `&&`, `!`, comparisons, `+ -`, `* / %`, unary
  // `+ -`, accessors). `||` and `&&` take predicates.
  private parseOr(): Parsed {
    let left = this.parseAnd();
    // Each `||` step deepens the left-deep tree by one level (json.md §6.4).
    let steps = 0;
    for (;;) {
      this.skipWs();
      if (!this.eatOp("||")) {
        this.depth -= steps;
        return left;
      }
      this.deepen();
      steps += 1;
      const a = needPred(left);
      const b = needPred(this.parseAnd());
      left = { kind: "pred", pred: { kind: "or", a, b } };
    }
  }

  private parseAnd(): Parsed {
    let left = this.parseNot();
    // Each `&&` step deepens the left-deep tree by one level (json.md §6.4).
    let steps = 0;
    for (;;) {
      this.skipWs();
      if (!this.eatOp("&&")) {
        this.depth -= steps;
        return left;
      }
      this.deepen();
      steps += 1;
      const a = needPred(left);
      const b = needPred(this.parseNot());
      left = { kind: "pred", pred: { kind: "and", a, b } };
    }
  }

  // parseNot parses `!(predicate)` — the operand must be a parenthesized predicate.
  private parseNot(): Parsed {
    this.skipWs();
    if (this.peek() === "!" && this.peekAt(1) !== "=") {
      this.i += 1;
      this.skipWs();
      if (this.eatKeyword("exists")) {
        return { kind: "pred", pred: { kind: "not", p: this.parseExists() } };
      }
      if (!this.eat("(")) {
        throw malformed("expected `(` after `!`");
      }
      this.deepen();
      const inner = needPred(this.parseOr());
      this.depth -= 1;
      this.skipWs();
      if (!this.eat(")")) {
        throw malformed("expected `)` after `!(`");
      }
      return { kind: "pred", pred: { kind: "not", p: inner } };
    }
    return this.parseComparison();
  }

  // parseExists parses `exists (expr)` after its keyword (jsonpath.md §2): the operand is an
  // expression, not a predicate.
  private parseExists(): Pred {
    this.skipWs();
    if (!this.eat("(")) throw malformed("expected `(` after exists");
    this.deepen();
    const e = needExpr(this.parseOr());
    this.depth -= 1;
    this.skipWs();
    if (!this.eat(")")) throw malformed("expected `)` after exists (");
    return { kind: "exists", e };
  }

  // parseComparison parses `expr cmp expr`, `expr starts with s`, `expr like_regex "p" [flag "f"]`,
  // `(predicate) is unknown` (repeatable), or a lone expression or predicate.
  private parseComparison(): Parsed {
    let left = this.parseAdditive();
    if (left.kind === "pred") {
      // `is unknown` applies only to a parenthesized predicate, and its result is one too.
      while (left.kind === "pred" && left.parenPred === true) {
        this.skipWs();
        if (!this.wordAhead()) break;
        const start = this.i;
        if (!keywordIs(this.scanWord(), "is")) {
          this.i = start;
          break;
        }
        this.skipWs();
        if (!this.eatKeyword("unknown")) throw malformed("expected `unknown` after `is`");
        // `(p) is unknown is unknown` is not PostgreSQL's grammar; only a parenthesized one nests.
        left = { kind: "pred", pred: { kind: "isUnknown", p: left.pred } };
      }
      return { kind: "pred", pred: left.pred };
    }
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
      // A word predicate operator. The name is lexed first, so its escape error wins; any other
      // name is trailing input.
      if (this.wordAhead()) {
        const start = this.i;
        const word = this.scanWord();
        if (keywordIs(word, "starts")) return this.parseStartsWith(left.expr);
        if (keywordIs(word, "like_regex")) return this.parseLikeRegex(left.expr);
        this.i = start;
      }
      return left;
    }
    const rhs = needExpr(this.parseAdditive());
    return { kind: "pred", pred: { kind: "compare", lhs: left.expr, op, rhs } };
  }

  // parseStartsWith parses the rest of `expr starts with s` after `starts`: s is a string literal or
  // a variable (jsonpath.md §2).
  private parseStartsWith(lhs: PathExpr): Parsed {
    this.skipWs();
    if (!this.eatKeyword("with")) throw malformed("expected `with` after `starts`");
    this.skipWs();
    let rhs: PathExpr;
    const c = this.peek();
    if (c === '"') {
      rhs = {
        primary: { kind: "lit", node: { kind: "string", value: this.parseQuoted() } },
        steps: [],
      };
    } else if (c === "$") {
      const v = this.parsePostfix();
      if (v.kind !== "expr" || v.expr.primary.kind !== "var" || v.expr.steps.length > 0) {
        throw malformed("starts with takes a string or a variable");
      }
      rhs = v.expr;
    } else {
      throw malformed("starts with takes a string or a variable");
    }
    return { kind: "pred", pred: { kind: "startsWith", lhs, rhs } };
  }

  // parseLikeRegex parses the rest of `expr like_regex "pattern" [flag "flags"]` after `like_regex`
  // and compiles the pattern (jsonpath.md §4.3): an unknown flag is 42601, `x` or `m` without `q`
  // 0A000, a malformed pattern 2201B.
  private parseLikeRegex(lhs: PathExpr): Parsed {
    this.skipWs();
    if (this.peek() !== '"') throw malformed("like_regex takes a string pattern");
    const pattern = this.parseQuoted();
    let flags = 0;
    const save = this.i;
    this.skipWs();
    if (this.eatKeyword("flag")) {
      this.skipWs();
      if (this.peek() !== '"') throw malformed("flag takes a string");
      for (const c of this.parseQuoted()) {
        const i = REGEX_FLAG_CHARS.indexOf(c);
        if (i < 0) {
          throw malformed(`unrecognized flag character "${c}" in LIKE_REGEX predicate`);
        }
        flags |= 1 << i;
      }
    } else {
      this.i = save;
    }
    if ((flags & REGEX_Q) === 0) {
      if ((flags & REGEX_X) !== 0) {
        throw engineError(
          "feature_not_supported",
          'XQuery "x" flag (expanded regular expressions) is not implemented',
        );
      }
    }
    const regex = compileLikeRegex(pattern, flags, this.prop);
    this.regexInsts += regexNinst(regex);
    return { kind: "pred", pred: { kind: "likeRegex", lhs, pattern, flags, regex } };
  }

  // parseAdditive parses `+` and `-`, left-associative.
  private parseAdditive(): Parsed {
    let left = this.parseMultiplicative();
    // Each operator deepens the left-deep tree by one level (json.md §6.4).
    let steps = 0;
    for (;;) {
      this.skipWs();
      const c = this.peek();
      if (c !== "+" && c !== "-") {
        this.depth -= steps;
        return left;
      }
      this.i += 1;
      this.deepen();
      steps += 1;
      const l = needExpr(left);
      const r = needExpr(this.parseMultiplicative());
      left = { kind: "expr", expr: { primary: { kind: "binary", op: c, l, r }, steps: [] } };
    }
  }

  // parseMultiplicative parses `*`, `/`, and `%`, left-associative.
  private parseMultiplicative(): Parsed {
    let left = this.parseUnary();
    let steps = 0;
    for (;;) {
      this.skipWs();
      const c = this.peek();
      if (c !== "*" && c !== "/" && c !== "%") {
        this.depth -= steps;
        return left;
      }
      this.i += 1;
      this.deepen();
      steps += 1;
      const l = needExpr(left);
      const r = needExpr(this.parseUnary());
      left = { kind: "expr", expr: { primary: { kind: "binary", op: c, l, r }, steps: [] } };
    }
  }

  // parseUnary parses unary `+` / `-`, which bind tighter than the binary operators but looser than
  // accessors (`-$.a * 2` is `(-($.a)) * 2`). Applied to a number literal they fold into it (`-1`),
  // as PostgreSQL's grammar does.
  private parseUnary(): Parsed {
    this.skipWs();
    const op = this.peek();
    if (op !== "+" && op !== "-") return this.parsePostfix();
    this.i += 1;
    this.deepen();
    const arg = needExpr(this.parseUnary());
    this.depth -= 1;
    if (
      arg.primary.kind === "lit" &&
      arg.primary.node.kind === "number" &&
      arg.steps.length === 0
    ) {
      if (op === "+") return { kind: "expr", expr: arg };
      const node: JsonNode = { kind: "number", dec: arg.primary.node.dec.negate() };
      return { kind: "expr", expr: { primary: { kind: "lit", node }, steps: [] } };
    }
    return { kind: "expr", expr: { primary: { kind: "unary", op, arg }, steps: [] } };
  }

  // Consume a multi-character operator token if it appears at the cursor.
  private eatOp(op: string): boolean {
    if (this.s.startsWith(op, this.i)) {
      this.i += op.length;
      return true;
    }
    return false;
  }

  // Parse a `"…"` jsonpath string — a port of PostgreSQL's jsonpath scanner escapes
  // (jsonpath_scan.l, jsonpath.md §2.1): `\b \f \n \r \t \v`; a run of `\uXXXX` / `\u{X…}` (1–6 hex
  // digits) escapes, within which a surrogate pair combines; `\xHH`; and a backslash before any other
  // character is that character. A decoded NUL is 22P05, an unpaired surrogate 22P02 (PG's Detail),
  // and a malformed `\u` / `\x` or a code point above U+10FFFF 42601.
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
      if (c !== "\\") {
        // Copy one character (a JS string char; surrogate pairs copy as two units — byte-equivalent
        // to Rust copying a UTF-8 char, since the renderer re-escapes by code point identically).
        out += c;
        this.i += 1;
        continue;
      }
      out += this.parseEscape();
    }
  }

  // parseEscape decodes the backslash escape at the cursor — shared by quoted strings and unquoted
  // names (jsonpath.md §2.1).
  private parseEscape(): string {
    const e = this.s[this.i + 1];
    if (e === undefined) throw malformed("unexpected end after backslash");
    if (e === "u") return this.parseUnicodeRun();
    if (e === "x") {
      const hex = this.s.slice(this.i + 2, this.i + 4);
      if (!/^[0-9a-fA-F]{2}$/.test(hex)) throw malformed("invalid hexadecimal character sequence");
      this.i += 4;
      return codePointText(Number.parseInt(hex, 16));
    }
    this.i += 2; // the backslash and the escaped character
    switch (e) {
      case "b":
        return "\b";
      case "f":
        return "\f";
      case "n":
        return "\n";
      case "r":
        return "\r";
      case "t":
        return "\t";
      case "v":
        return "\v";
      default:
        return e; // any other escaped character is itself (a pair's low half copies next)
    }
  }

  // A run of consecutive `\u` escapes (the cursor at the first `\`) — PG's `{unicode}+` token, lexed
  // whole before any is decoded, so a malformed escape anywhere in the run is 42601 ahead of a decode
  // error. Decoding is PG's addUnicode: a high surrogate must be followed, within the run, by a low
  // one (the pair combines); a lone or doubled surrogate is 22P02.
  private parseUnicodeRun(): string {
    const units: number[] = [];
    while (this.s[this.i] === "\\" && this.s[this.i + 1] === "u") {
      this.i += 2;
      const braced = this.eat("{");
      const start = this.i;
      const max = braced ? 6 : 4;
      while (this.i - start < max && /^[0-9a-fA-F]$/.test(this.s[this.i] ?? "")) this.i += 1;
      const digits = this.i - start;
      if ((braced && (digits === 0 || !this.eat("}"))) || (!braced && digits < 4)) {
        throw malformed("invalid Unicode escape sequence");
      }
      units.push(Number.parseInt(this.s.slice(start, start + digits), 16));
    }
    let out = "";
    let hiSurrogate = -1;
    for (let ch of units) {
      if (ch >= 0xd800 && ch <= 0xdbff) {
        if (hiSurrogate !== -1) throw escapeSyntaxError("jsonpath", HIGH_SURROGATE_DETAIL);
        hiSurrogate = ch;
        continue;
      } else if (ch >= 0xdc00 && ch <= 0xdfff) {
        if (hiSurrogate === -1) throw escapeSyntaxError("jsonpath", LOW_SURROGATE_DETAIL);
        ch = 0x10000 + (((hiSurrogate - 0xd800) << 10) | (ch - 0xdc00));
        hiSurrogate = -1;
      } else if (hiSurrogate !== -1) {
        throw escapeSyntaxError("jsonpath", LOW_SURROGATE_DETAIL);
      }
      out += codePointText(ch);
    }
    if (hiSurrogate !== -1) throw escapeSyntaxError("jsonpath", LOW_SURROGATE_DETAIL);
    return out;
  }

  // Parse a `[…]` subscript list (the opening `[` consumed, not the wildcard form). Each subscript
  // is `index` or `index to index`, an index any expression (`1`, `last - 1`, `$.i`), in which
  // `last` is allowed.
  private parseSubscripts(): Subscript[] {
    const subs: Subscript[] = [];
    this.deepen();
    this.inSubscript += 1;
    for (;;) {
      const a = this.parseIndex();
      this.skipWs();
      let sub: Subscript;
      if (this.eatKeyword("to")) {
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
        this.inSubscript -= 1;
        this.depth -= 1;
        return subs;
      }
      throw malformed("expected `,` or `]` in subscript");
    }
  }

  // parseIndex parses one subscript index: a number literal or `last` keeps its own form; anything
  // else is an expression evaluated per array.
  private parseIndex(): Index {
    this.skipWs();
    if (this.peek() === undefined) throw malformed("expected a subscript index");
    const e = needExpr(this.parseAdditive());
    if (e.steps.length === 0) {
      if (e.primary.kind === "lit" && e.primary.node.kind === "number") {
        return { kind: "number", value: e.primary.node.dec };
      }
      if (e.primary.kind === "last") return { kind: "last" };
    }
    return { kind: "expr", expr: e };
  }
}

// compileLikeRegex compiles a like_regex pattern under its flags (jsonpath.md §4.3, regex.md §8d):
// `i` folds the pattern, `q` quotes it, `s` is dot-all (otherwise the mode is newline-sensitive),
// and `m` makes `^`/`$` line anchors.
// A quoted pattern is all literal, so `i` folds every character of it; otherwise the fold keeps the
// letter after a backslash (regex.md §1).
function compileLikeRegex(
  pattern: string,
  flags: number,
  prop: PropertyTable | undefined,
): RegexProgram {
  let p = pattern;
  if ((flags & REGEX_I) !== 0) {
    p = (flags & REGEX_Q) !== 0 ? foldLowerSimple(pattern, prop) : foldRegexPattern(pattern, prop);
  }
  if ((flags & REGEX_Q) !== 0) {
    return compileRegexMode(quoteRegex(p), { nlStop: false, dotAll: false, nlAnchor: false });
  }
  // PostgreSQL's jspConvertRegexFlags: no `s` adds REG_NLSTOP, and `m` adds REG_NLANCH,
  // independently of each other.
  const dotAll = (flags & REGEX_S) !== 0;
  return compileRegexMode(p, { nlStop: !dotAll, dotAll, nlAnchor: (flags & REGEX_M) !== 0 });
}

// compile compiles a jsonpath source string. Malformed → `42601`; a valid-PG but unsupported
// construct → `0A000`. prop is the handle environment's casing table (undefined ⇒ ASCII).
export function compile(src: string, prop: PropertyTable | undefined): JsonPath {
  const path = new Parser(src, prop).compile();
  // A path is evaluated from its canonical text, whose parenthesization can nest deeper than the
  // source (`1 + 1 + 1` renders `((1 + 1) + 1)`), so that text must compile too (§8).
  new Parser(render(path), prop).compile();
  return path;
}

// render is the canonical render (spec/design/jsonpath.md §2.2): `strict` kept / `lax` omitted;
// member keys quoted; `[*]`, `[i]`, `[i to j]` subscripts; methods lowercase; operators spaced and
// parenthesized by PostgreSQL's `jsonpath_out` rules. The top level parenthesizes an operator
// (`($."a" + 1)`, `($."a" == 1)`), as PostgreSQL prints it.
export function render(jp: JsonPath): string {
  const out = jp.strict ? "strict " : "";
  if (jp.body.kind === "path") {
    return out + writeExpr(jp.body.expr, true);
  }
  return out + writePred(jp.body.pred, true);
}

// exprPriority is PostgreSQL's operationPriority of an expression's head: `||` 0, `&&` 1,
// comparisons 2, `+ -` 3, `* / %` 4, unary `+ -` 5, anything else 6.
function exprPriority(e: PathExpr): number {
  switch (e.primary.kind) {
    case "unary":
      return 5;
    case "binary":
      return arithPriority(e.primary.op);
    case "pred":
      return predPriority(e.primary.pred);
    default:
      return 6;
  }
}

function predPriority(pred: Pred): number {
  switch (pred.kind) {
    case "or":
      return 0;
    case "and":
      return 1;
    case "compare":
    case "startsWith":
      return 2;
    default:
      return 6;
  }
}

// writeExpr renders an expression — PostgreSQL's printJsonPathItem. An operator is parenthesized
// when brackets is set: at the top level, and for an operand that binds no tighter than its parent
// (`((1 + 2) + 3)`, `(1 - (2 - 3))`). jed also parenthesizes an operator followed by accessors,
// which PostgreSQL prints bare in an operand (`$."a" + 1.type()`), a text that does not read back as
// the same path (§2.2). A number literal followed by a step is parenthesized (`(1.5).type()`).
function writeExpr(e: PathExpr, brackets: boolean): string {
  const wrap = brackets || e.steps.length > 0;
  const p = e.primary;
  let out: string;
  switch (p.kind) {
    case "root":
      out = "$";
      break;
    case "current":
      out = "@";
      break;
    case "last":
      out = "last";
      break;
    case "var":
      out = "$" + writeQuoted(p.name);
      break;
    case "lit":
      out = jsonCompactOut(p.node);
      if (p.node.kind === "number" && e.steps.length > 0) out = "(" + out + ")";
      break;
    case "unary":
      out = p.op + writeExpr(p.arg, exprPriority(p.arg) <= 5);
      if (wrap) out = "(" + out + ")";
      break;
    case "binary": {
      const prio = arithPriority(p.op);
      out =
        writeExpr(p.l, exprPriority(p.l) <= prio) +
        " " +
        p.op +
        " " +
        writeExpr(p.r, exprPriority(p.r) <= prio);
      if (wrap) out = "(" + out + ")";
      break;
    }
    case "pred":
      // A predicate that prints without its own brackets (`!(…)`, `exists (…)`, `(…) is unknown`)
      // keeps the parentheses that make it an operand of the accessors after it (jsonpath.md §2.3).
      if (p.pred.kind === "not" || p.pred.kind === "exists" || p.pred.kind === "isUnknown") {
        out = "(" + writePred(p.pred, false) + ")";
      } else {
        out = writePred(p.pred, wrap);
      }
      break;
  }
  return out + writeSteps(e.steps);
}

// writeSteps renders an accessor-step sequence. Port of impl/rust/src/jsonpath.rs `write_steps`.
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
        out += writePred(step.pred, false);
        out += ")";
        break;
      case "method":
        out += "." + step.method.name + "(";
        if (step.method.precision !== null) out += step.method.precision.render();
        if (step.method.scale !== null) out += "," + step.method.scale.render();
        out += ")";
        break;
      case "any":
        out += writeAnyLevels(step.first, step.last);
        break;
    }
  }
  return out;
}

// writeAnyLevels renders a `.**` step as PostgreSQL's printJsonPathItem does: `{0 to last}` is bare,
// equal bounds print once.
function writeAnyLevels(first: number, last: number): string {
  const level = (v: number): string => (v === LEVEL_LAST ? "last" : String(v));
  if (first === 0 && last === LEVEL_LAST) return ".**";
  if (first === last) return `.**{${level(first)}}`;
  return `.**{${level(first)} to ${level(last)}}`;
}

// writePred renders a predicate (PostgreSQL's form: `&&`/`||` and `a op b` spaced, `!(…)`),
// parenthesized when brackets is set and its operands by priority, like writeExpr.
function writePred(pred: Pred, brackets: boolean): string {
  let out: string;
  switch (pred.kind) {
    case "or":
    case "and": {
      const prio = predPriority(pred);
      out =
        writePred(pred.a, predPriority(pred.a) <= prio) +
        (pred.kind === "or" ? " || " : " && ") +
        writePred(pred.b, predPriority(pred.b) <= prio);
      break;
    }
    case "not":
      return "!(" + writePred(pred.p, false) + ")";
    case "exists":
      return "exists (" + writeExpr(pred.e, false) + ")";
    case "isUnknown":
      return "(" + writePred(pred.p, false) + ") is unknown";
    case "startsWith":
      out =
        writeExpr(pred.lhs, exprPriority(pred.lhs) <= 2) +
        " starts with " +
        writeExpr(pred.rhs, exprPriority(pred.rhs) <= 2);
      break;
    case "likeRegex": {
      // PostgreSQL brackets the operand by priority against like_regex's 6, so an operator operand
      // is always wrapped.
      out = writeExpr(pred.lhs, true) + " like_regex " + writeQuoted(pred.pattern);
      if (pred.flags !== 0) {
        out += ' flag "';
        for (let i = 0; i < REGEX_FLAG_CHARS.length; i++) {
          if ((pred.flags & (1 << i)) !== 0) out += REGEX_FLAG_CHARS[i];
        }
        out += '"';
      }
      break;
    }
    case "compare":
      out =
        writeExpr(pred.lhs, exprPriority(pred.lhs) <= 2) +
        " " +
        cmpOpSymbol(pred.op) +
        " " +
        writeExpr(pred.rhs, exprPriority(pred.rhs) <= 2);
      break;
  }
  return brackets ? "(" + out + ")" : out;
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

function writeIndex(i: Index): string {
  switch (i.kind) {
    case "number":
      return i.value.render();
    case "last":
      return "last";
    case "expr":
      return writeExpr(i.expr, false);
  }
}

// writeQuoted renders a member key as a canonical jsonpath quoted string — PostgreSQL's escape_json:
// `\"`, `\\`, `\b \f \n \r \t`, and any other control character as `\u00XX`.
function writeQuoted(k: string): string {
  let out = '"';
  for (const c of k) {
    const code = c.codePointAt(0)!;
    if (c === '"') {
      out += '\\"';
    } else if (c === "\\") {
      out += "\\\\";
    } else if (c === "\b") {
      out += "\\b";
    } else if (c === "\f") {
      out += "\\f";
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

// ---------------------------------------------------------------------------------------------
// Evaluation (jsonpath.md §3-4) — the lax/strict ordered SQL/JSON item sequence.
// ---------------------------------------------------------------------------------------------

// EvalOpts is how a caller consumes the sequence (jsonpath.md §5).
export interface EvalOpts {
  // An existence test (jsonb_path_exists, @?, JSON_EXISTS): a lax path stops at its first item, so a
  // later item's error is never reached. A strict path evaluates fully (§4.2).
  exists: boolean;
  // The variables the path's `$name` references read (§5).
  vars: Vars;
}

// Vars is what a path's `$name` references are bound to (jsonpath.md §5), and the variable base
// objects they count for .keyvalue() ids (§3.1):
//   - none: no variables — the @? / @@ operators. Counts no base object.
//   - object: the jsonb_path_* functions' vars object (already checked to be an object by
//     checkVarsObject). It is one base object, number 1.
//   - passing: the SQL/JSON functions' PASSING values, as passingVars orders them. Each is a base
//     object numbered by its position in the clause; count is the clause's length.
export type Vars =
  | { kind: "none" }
  | { kind: "object"; node: JsonNode }
  | { kind: "passing"; vars: PassingVar[]; count: number };

// NO_VARS binds no variables (the @? / @@ operators).
export const NO_VARS: Vars = { kind: "none" };

// varsCount is how many variable base objects the caller supplies (PostgreSQL's countVars): the
// first .keyvalue() object is numbered one past them.
function varsCount(v: Vars): number {
  switch (v.kind) {
    case "none":
      return 0;
    case "object":
      return 1;
    case "passing":
      return v.count;
  }
}

// PassingVar is one `PASSING value AS name` binding (jsonpath.md §5): the value as a jsonb item and
// its base object number (its 1-based position in the clause).
export interface PassingVar {
  name: string;
  value: JsonNode;
  id: bigint;
}

// passingVars orders a PASSING clause's bindings for lookup: sorted by name (UTF-8 bytes), and of
// two bindings with the same name only the first kept, as PostgreSQL's lookup finds the first. Each
// keeps its clause position as its base object number.
export function passingVars(bindings: { name: string; value: JsonNode }[]): PassingVar[] {
  const vars: PassingVar[] = bindings.map((b, i) => ({
    name: b.name,
    value: b.value,
    id: BigInt(i + 1),
  }));
  // Array.prototype.sort is stable, so the first of equal names stays first.
  vars.sort((a, b) => compareTextC(a.name, b.name));
  const out: PassingVar[] = [];
  for (const v of vars) {
    if (out.length > 0 && out[out.length - 1]!.name === v.name) continue;
    out.push(v);
  }
  return out;
}

// checkVarsObject checks a jsonb_path_* function's vars argument (PostgreSQL's
// countVariablesFromJsonb): it must be an object, else 22023. Checked before evaluation, so it is
// raised even when the path uses no variable and even in silent mode.
export function checkVarsObject(vars: JsonNode): void {
  if (vars.kind === "object") return;
  throw engineError("invalid_parameter_value", '"vars" argument is not an object').withDetail(
    'Jsonpath parameters should be encoded as key-value pairs of "vars" object.',
  );
}

// compileMetered compiles a path for evaluation, charging jsonpath_compile for each UTF-8 byte of
// the text first (jsonpath.md §7). Every evaluation compiles its path; there is no cache.
export function compileMetered(src: string, m: Meter, prop: PropertyTable | undefined): JsonPath {
  m.charge(COSTS.jsonpathCompile * BigInt(utf8Length(src)));
  m.guard();
  // The text is a jsonpath value's canonical text (or a JSON_TABLE path), already checked by
  // compile at resolve.
  const path = new Parser(src, prop).compile();
  // Each like_regex pattern was compiled with the path (jsonpath.md §7).
  m.charge(COSTS.regexCompile * BigInt(path.regexInsts));
  m.guard();
  return path;
}

// compileCastMetered compiles the per-row text of a runtime text → jsonpath cast (casts.toml): the
// full resolve-time compile (the canonical text must compile too — §8), metered as compileMetered.
export function compileCastMetered(
  src: string,
  m: Meter,
  prop: PropertyTable | undefined,
): JsonPath {
  m.charge(COSTS.jsonpathCompile * BigInt(utf8Length(src)));
  m.guard();
  const path = compile(src, prop);
  m.charge(COSTS.regexCompile * BigInt(path.regexInsts));
  m.guard();
  return path;
}

// evalPath evaluates a compiled path over a jsonb context item → the ordered SQL/JSON sequence
// (jsonpath.md §3). Generated items (method results, .keyvalue() objects, literals) are built; any
// error propagates. Metered by jsonpath_step (§7). Port of impl/rust/src/jsonpath.rs `eval`.
export function evalPath(path: JsonPath, ctx: JsonNode, opts: EvalOpts, m: Meter): JsonNode[] {
  const items: Item[] = [];
  evalItems(path, ctx, opts, m, items);
  return items.map(materialize);
}

// evalPathSilent is evalPath in PostgreSQL's silent mode (@?, @@): a SQL/JSON (class 22) error ends
// the evaluation, returning the items found before it and `errored: true`. Any other error
// propagates.
export function evalPathSilent(
  path: JsonPath,
  ctx: JsonNode,
  opts: EvalOpts,
  m: Meter,
): { seq: JsonNode[]; errored: boolean } {
  const items: Item[] = [];
  let errored = false;
  try {
    evalItems(path, ctx, opts, m, items);
  } catch (e) {
    if (!isSoft(e)) throw e;
    errored = true;
  }
  return { seq: items.map(materialize), errored };
}

// isSoft is a SQL/JSON (class 22) error: the kind silent mode and filter operands suppress. A cost,
// budget, or cancellation error is never suppressed (§7).
function isSoft(e: unknown): boolean {
  return e instanceof EngineError && e.code().startsWith("22");
}

function evalItems(path: JsonPath, ctx: JsonNode, opts: EvalOpts, m: Meter, out: Item[]): void {
  const cx: Ctx = {
    root: ctx,
    vars: opts.vars,
    strict: path.strict,
    ignore: false,
    m,
    prop: path.prop,
    nextGen: 1n + BigInt(varsCount(opts.vars)),
    offsets: new Map(),
  };
  if (path.body.kind === "predicate") {
    // A top-level predicate → a single item: its truth value as a boolean, or JSON null when unknown
    // (§4.4).
    const truth = evalPred(cx, path.body.pred, { kind: "doc", node: ctx }, ROOT_BASE, null);
    out.push({
      kind: "val",
      node: truth === null ? { kind: "null" } : { kind: "bool", value: truth },
    });
    return;
  }
  // `@` is not allowed outside a filter, so the root expression's current item is unused.
  const current: Item = { kind: "doc", node: ctx };
  runExpr(cx, path.body.expr, current, ROOT_BASE, null, opts.exists && !path.strict, out);
}

// One item of the SQL/JSON sequence: a node of the context document, a scalar the evaluation
// produced (a path literal or an item-method result), or a .keyvalue() object (materialized only on
// output).
type Item =
  | { kind: "doc"; node: JsonNode }
  | { kind: "val"; node: JsonNode }
  | { kind: "kv"; kv: KvPair };

// One .keyvalue() pair object (jsonpath.md §3.1). `id` is the "id" member (the source object's id);
// `generated` is this object's own base-object number — the id prefix of a .keyvalue() applied
// within it. `tree` is the base object (the document or a variable's) whose binary layout the
// value's containers are measured in.
interface KvPair {
  key: string;
  value: Item;
  id: bigint;
  generated: bigint;
  tree: JsonNode;
}

// The base object a .keyvalue() id is measured against (jsonpath.md §3.1): the context item (kv and
// varTree null) while a $ path runs, a variable's base object (varTree, numbered varId — the vars
// object, number 1, or a PASSING value, its position) while a $name path runs, or the .keyvalue()
// object whose continuation is running.
interface Base {
  kv: KvPair | null;
  varTree: JsonNode | null;
  varId: bigint;
}

const ROOT_BASE: Base = { kv: null, varTree: null, varId: 0n };

// The evaluation context shared by a path and the filter operands nested in it.
interface Ctx {
  root: JsonNode;
  vars: Vars;
  strict: boolean;
  // ignore suppresses navigation errors while the rest of a path below `.**` runs, in either mode
  // (jsonpath.md §3.3) — PostgreSQL's ignoreStructuralErrors, which lax always sets.
  ignore: boolean;
  m: Meter;
  // The casing table an `i`-flagged like_regex folds its subject under (the path's, undefined ⇒ ASCII).
  prop: PropertyTable | undefined;
  // The next .keyvalue() object number, assigned in evaluation order.
  nextGen: bigint;
  // Per base object (the document or a variable's), each container's byte offset in PostgreSQL's
  // binary jsonb layout of it, computed for a base object on the first .keyvalue() that needs one.
  offsets: Map<JsonNode, Map<JsonNode, number>>;
}

// structural is whether a navigation failure raises: strict, and not below a `.**` (§3.3, §4.2).
function structural(cx: Ctx): boolean {
  return cx.strict && !cx.ignore;
}

// charge charges n units of jsonpath_step and guards (jsonpath.md §7).
function charge(cx: Ctx, n: number): void {
  cx.m.charge(COSTS.jsonpathStep * BigInt(n));
  cx.m.guard();
}

// scalarNode is the node of a document or generated item (null for a .keyvalue() object).
function scalarNode(item: Item): JsonNode | null {
  return item.kind === "kv" ? null : item.node;
}

function docArray(item: Item): JsonNode[] | null {
  return item.kind === "doc" && item.node.kind === "array" ? item.node.elements : null;
}

function typeName(item: Item): string {
  return item.kind === "kv" ? "object" : typeofName(item.node);
}

// materialize is the item as a jsonb value; a .keyvalue() object is built.
function materialize(item: Item): JsonNode {
  if (item.kind !== "kv") return item.node;
  return {
    kind: "object",
    members: [
      { key: "id", value: { kind: "number", dec: Decimal.fromBigInt(item.kv.id) } },
      { key: "key", value: { kind: "string", value: item.kv.key } },
      { key: "value", value: materialize(item.kv.value) },
    ],
  };
}

// A step application in progress: step `step` applied to `item`, producing its outputs lazily so
// each output's continuation runs before the next output is produced (PostgreSQL's depth-first
// order, which fixes the error raised first, the early stop of an existence test, and the
// .keyvalue() numbering). `unwrap` is whether lax auto-unwrapping applies (§4.1) — false for the
// elements of an unwrapped array, which are not unwrapped again.
interface Frame {
  step: number;
  item: Item;
  base: Base;
  unwrap: boolean;
  started: boolean;
  a: number;
  b: number;
  // The current subscript's resolved element range end (null between subscripts).
  to: number | null;
  // The .keyvalue() id of this frame's object.
  kvId: bigint;
  // The size of the innermost array being subscripted, which `last` refers to (in a filter of this
  // step) — set by an enclosing subscript step, for as long as its outputs' continuations run, as in
  // PostgreSQL.
  last: number | null;
  // A subscript step's array size: its own outputs' `last`.
  size: number;
  // Whether this frame runs below a `.**`, suppressing navigation errors (§3.3).
  ignore: boolean;
  // A `.**` step's descent: the containers being walked, innermost last.
  walk: AnyWalk[];
}

// One container a `.**` step is walking: its children are at `level`, and `next` is the next
// child's position.
interface AnyWalk {
  item: Item;
  level: number;
  next: number;
}

function newFrame(
  step: number,
  item: Item,
  base: Base,
  unwrap: boolean,
  last: number | null,
  ignore: boolean,
): Frame {
  return {
    step,
    item,
    base,
    unwrap,
    started: false,
    a: 0,
    b: 0,
    to: null,
    kvId: 0n,
    last,
    size: 0,
    ignore,
    walk: [],
  };
}

// What a frame produces next: an item for the following step (or the result) with its
// continuation's base, one element of a lax-unwrapped array to apply the frame's step to, or done.
type Out =
  | { kind: "item"; item: Item; base: Base }
  | { kind: "sub"; frame: Frame }
  | { kind: "done" };

const DONE: Out = { kind: "done" };

// runExpr runs an expression depth-first (§3): its head produces items one at a time (§3.2), and each
// runs through the accessor steps on an explicit stack (no native recursion per step) before the head
// produces the next. `current` is the item `@` names and `last` the size of the innermost array being
// subscripted. Each final item is appended to out; stopAtFirst ends the run at the first one —
// PostgreSQL's lax existence test, which also lets a unary operator skip a non-number.
function runExpr(
  cx: Ctx,
  e: PathExpr,
  current: Item,
  base: Base,
  last: number | null,
  stopAtFirst: boolean,
  out: Item[],
): void {
  const steps = e.steps;
  const unwrap = !cx.strict;
  const head: Head = { started: false, items: [], at: 0 };
  const stack: Frame[] = [];
  for (;;) {
    const top = stack[stack.length - 1];
    if (top === undefined) {
      const h = headNext(cx, e, head, current, base, last, stopAtFirst);
      if (h === null) return;
      if (steps.length === 0) {
        out.push(h.item);
        if (stopAtFirst) return;
      } else {
        stack.push(newFrame(0, h.item, h.base, unwrap, last, cx.ignore));
      }
      continue;
    }
    // The frame's step runs with its own suppression of navigation errors, which reaches any filter
    // operand it evaluates (§3.3).
    const saved = cx.ignore;
    cx.ignore = top.ignore;
    let o: Out;
    try {
      o = nextOut(cx, steps, current, top);
    } finally {
      cx.ignore = saved;
    }
    if (o.kind === "item") {
      const next = top.step + 1;
      if (next === steps.length) {
        out.push(o.item);
        if (stopAtFirst) return;
      } else {
        // A subscript step's outputs run their continuations with `last` naming its array.
        const childLast = steps[top.step]!.kind === "subscripts" ? top.size : top.last;
        // Every item a `.**` emits runs the rest of the path with navigation errors suppressed.
        const ignore = top.ignore || steps[top.step]!.kind === "any";
        stack.push(newFrame(next, o.item, o.base, unwrap, childLast, ignore));
      }
    } else if (o.kind === "sub") {
      stack.push(o.frame);
    } else {
      stack.pop();
    }
  }
}

// The state of an expression's head: whether it has started, and a unary operator's operand items
// with the next one's position.
interface Head {
  started: boolean;
  items: Item[];
  at: number;
}

// headNext produces the next item of an expression's head (`$`, `@`, a literal, `last`, an operator,
// or a parenthesized predicate), with the base its continuation runs under; null when there is none.
function headNext(
  cx: Ctx,
  e: PathExpr,
  head: Head,
  current: Item,
  base: Base,
  last: number | null,
  stopAtFirst: boolean,
): { item: Item; base: Base } | null {
  const p = e.primary;
  if (p.kind === "unary") {
    // Each item of the operand (lax: an array item unwrapped one level) must be a number: one
    // application each, one emitted item for each number. A lax existence test with nothing after
    // the operator skips a non-number, as PostgreSQL does.
    if (!head.started) {
      head.started = true;
      head.items = evalSeq(cx, p.arg, current, base, last, true);
    }
    while (head.at < head.items.length) {
      const item = head.items[head.at]!;
      head.at += 1;
      charge(cx, 1);
      const n = scalarNode(item);
      if (n === null || n.kind !== "number") {
        if (stopAtFirst && e.steps.length === 0) continue;
        throw engineError(
          "sql_json_number_not_found",
          `operand of unary jsonpath operator ${p.op} is not a numeric value`,
        );
      }
      charge(cx, 1);
      if (p.op === "+") return { item, base };
      return { item: numberItem(n.dec.negate()), base };
    }
    return null;
  }
  if (head.started) return null;
  head.started = true;
  switch (p.kind) {
    case "root":
      return { item: { kind: "doc", node: cx.root }, base: ROOT_BASE };
    case "var":
      return variable(cx, p.name);
    case "current":
      return { item: current, base };
    case "lit":
      return { item: { kind: "val", node: p.node }, base };
    case "last":
      return { item: numberItem(Decimal.fromBigInt(BigInt((last ?? 0) - 1))), base };
    case "binary":
      return { item: evalBinary(cx, p.op, p.l, p.r, current, base, last), base };
    case "pred": {
      // `(predicate)` followed by accessors: the truth value as an item (JSON null for unknown).
      const truth = evalPred(cx, p.pred, current, base, last);
      return {
        item: {
          kind: "val",
          node: truth === null ? { kind: "null" } : { kind: "bool", value: truth },
        },
        base,
      };
    }
  }
}

// evalSeq evaluates an expression to its whole item sequence. With unwrap in lax mode an array item
// is unwrapped one level (§4.1 rule 4) — free: it only changes which items the caller sees.
function evalSeq(
  cx: Ctx,
  e: PathExpr,
  current: Item,
  base: Base,
  last: number | null,
  unwrap: boolean,
): Item[] {
  const seq: Item[] = [];
  runExpr(cx, e, current, base, last, false, seq);
  if (cx.strict || !unwrap) return seq;
  const out: Item[] = [];
  for (const item of seq) {
    const elems = docArray(item);
    if (elems === null) {
      out.push(item);
    } else {
      for (const node of elems) out.push({ kind: "doc", node });
    }
  }
  return out;
}

// evalBinary is `l op r` (§3.2): both operands are evaluated (lax unwraps an array item one level),
// then each must be a single number (22038). One application, the size-scaled decimal_work of the SQL
// operator, and one emitted item; the arithmetic is decimal's, so `/` takes PostgreSQL's
// select_div_scale and a zero divisor raises 22012.
function evalBinary(
  cx: Ctx,
  op: ArithOp,
  l: PathExpr,
  r: PathExpr,
  current: Item,
  base: Base,
  last: number | null,
): Item {
  const ls = evalSeq(cx, l, current, base, last, true);
  const rs = evalSeq(cx, r, current, base, last, true);
  charge(cx, 1);
  const single = (seq: Item[], side: string): Decimal => {
    const n = seq.length === 1 ? scalarNode(seq[0]!) : null;
    if (n === null || n.kind !== "number") {
      throw engineError(
        "singleton_sql_json_item_required",
        `${side} operand of jsonpath operator ${op} is not a single numeric value`,
      );
    }
    return n.dec;
  };
  const a = single(ls, "left");
  const b = single(rs, "right");
  let work: number;
  switch (op) {
    case "+":
    case "-":
      work = workLinear(a, b);
      break;
    case "*":
      work = workMul(a, b);
      break;
    case "/":
      work = workDiv(a, b);
      break;
    case "%":
      work = workMod(a, b);
      break;
  }
  cx.m.charge(COSTS.decimalWork * BigInt(work - 1));
  cx.m.guard();
  let v: Decimal;
  switch (op) {
    case "+":
      v = a.add(b);
      break;
    case "-":
      v = a.sub(b);
      break;
    case "*":
      v = a.mul(b);
      break;
    case "/":
      v = a.div(b);
      break;
    case "%":
      v = a.rem(b);
      break;
  }
  charge(cx, 1);
  return numberItem(v);
}

// unwraps reports whether lax mode unwraps an array before applying this step (§4.1 rule 1). The
// subscripts handle arrays themselves, and .type() / .size() describe the array.
function unwraps(step: Step): boolean {
  switch (step.kind) {
    case "member":
    case "wildcardMember":
    case "filter":
      return true;
    case "method":
      return step.method.name !== "type" && step.method.name !== "size";
    default:
      return false;
  }
}

// nextOut produces a frame's next output, charging its step application and each emitted item (§7).
function nextOut(cx: Ctx, steps: Step[], current: Item, f: Frame): Out {
  const step = steps[f.step]!;
  const unwrapped = f.unwrap && unwraps(step) ? docArray(f.item) : null;
  if (unwrapped !== null) {
    // lax: apply the step to each element instead (one level — an element array is not unwrapped
    // again).
    if (f.a < unwrapped.length) {
      const e = unwrapped[f.a]!;
      f.a += 1;
      return {
        kind: "sub",
        frame: newFrame(f.step, { kind: "doc", node: e }, f.base, false, f.last, f.ignore),
      };
    }
    return DONE;
  }
  const strict = cx.strict;
  let child: Item | null = null;
  switch (step.kind) {
    case "member": {
      if (f.started) return DONE;
      f.started = true;
      charge(cx, 1);
      child = memberOf(cx, f.item, step.key);
      break;
    }
    case "wildcardMember": {
      if (!f.started) {
        f.started = true;
        charge(cx, 1);
        if (typeName(f.item) !== "object") {
          if (structural(cx)) {
            throw engineError(
              "sql_json_object_not_found",
              "jsonpath wildcard member accessor can only be applied to an object",
            );
          }
          return DONE;
        }
      }
      const i = f.a;
      f.a += 1;
      if (f.item.kind === "doc" && f.item.node.kind === "object") {
        const member = f.item.node.members[i];
        if (member !== undefined) child = { kind: "doc", node: member.value };
      } else if (f.item.kind === "kv") {
        child = kvMember(f.item.kv, i);
      }
      break;
    }
    case "subscripts": {
      if (!f.started) {
        f.started = true;
        charge(cx, 1);
        if (strict && docArray(f.item) === null) {
          if (structural(cx)) {
            throw engineError(
              "sql_json_array_not_found",
              "jsonpath array accessor can only be applied to an array",
            );
          }
          // Strict below `.**`: no singleton, no item (§3.3).
          return DONE;
        }
      }
      // A non-array is a singleton array in lax (§4.1 rule 2).
      const array = docArray(f.item);
      const size = array === null ? 1 : array.length;
      f.size = size;
      while (child === null) {
        if (f.to === null) {
          const sub = step.subs[f.a];
          if (sub === undefined) return DONE;
          const [from, to] = subscriptRange(cx, sub, size, current, f.base);
          f.to = to;
          f.b = from;
        }
        if (f.b <= f.to) {
          const i = f.b;
          f.b += 1;
          child = array === null ? f.item : { kind: "doc", node: array[i]! };
        } else {
          f.to = null;
          f.a += 1;
        }
      }
      break;
    }
    case "wildcardElement": {
      const array = docArray(f.item);
      if (!f.started) {
        f.started = true;
        charge(cx, 1);
        if (array === null) {
          if (structural(cx)) {
            throw engineError(
              "sql_json_array_not_found",
              "jsonpath wildcard array accessor can only be applied to an array",
            );
          }
          // Strict below `.**`: no singleton, no item (§3.3).
          if (strict) return DONE;
          // lax: the item is a singleton array.
          charge(cx, 1);
          return { kind: "item", item: f.item, base: f.base };
        }
      }
      if (array !== null && f.a < array.length) {
        child = { kind: "doc", node: array[f.a]! };
        f.a += 1;
      }
      break;
    }
    case "filter": {
      // `?(predicate)` — keep the item when the predicate is definitely TRUE (§4). The predicate's
      // `@` is the item; `$` is the context item.
      if (f.started) return DONE;
      f.started = true;
      charge(cx, 1);
      if (evalPred(cx, step.pred, f.item, f.base, f.last) === true) child = f.item;
      break;
    }
    case "method": {
      if (step.method.name === "keyvalue") return nextKeyValue(cx, f);
      if (f.started) return DONE;
      f.started = true;
      charge(cx, 1);
      if (step.method.name === "size" && strict && !structural(cx) && docArray(f.item) === null) {
        // Strict .size() of a non-array below `.**`: no item (§3.3).
        return DONE;
      }
      child = applyMethod(step.method, f.item, strict);
      break;
    }
    case "any":
      return nextAny(cx, step, f);
  }
  if (child === null) return DONE;
  charge(cx, 1);
  return { kind: "item", item: child, base: f.base };
}

// nextAny produces a `.**` frame's next output (jsonpath.md §3.3): the item itself when the first
// level is 0, then its descendants in preorder — a child is emitted (when its level is in bounds)
// before the walk descends into it. Applying the step charges 1, each descendant visited 1, and each
// emitted item 1 (§7 rule 8).
function nextAny(cx: Ctx, step: { first: number; last: number }, f: Frame): Out {
  if (!f.started) {
    f.started = true;
    charge(cx, 1);
    if (step.last >= 1 && isContainer(f.item)) f.walk.push({ item: f.item, level: 1, next: 0 });
    if (step.first === 0) {
      charge(cx, 1);
      return { kind: "item", item: f.item, base: f.base };
    }
  }
  for (;;) {
    const top = f.walk[f.walk.length - 1];
    if (top === undefined) return DONE;
    const child = childAt(top.item, top.next);
    if (child === null) {
      f.walk.pop();
      continue;
    }
    top.next += 1;
    const level = top.level;
    charge(cx, 1);
    const container = isContainer(child);
    if (level < step.last && container) f.walk.push({ item: child, level: level + 1, next: 0 });
    // `{last}` (both bounds the largest level) is PostgreSQL's leaves-only walk.
    if (
      level >= step.first ||
      (step.first === LEVEL_LAST && step.last === LEVEL_LAST && !container)
    ) {
      charge(cx, 1);
      return { kind: "item", item: child, base: f.base };
    }
  }
}

// isContainer is whether an item has children for `.**`: a document array or object, or a
// .keyvalue() object.
function isContainer(item: Item): boolean {
  return (
    item.kind === "kv" ||
    (item.kind === "doc" && (item.node.kind === "array" || item.node.kind === "object"))
  );
}

// childAt is a container's i-th child in stored order: an element, a member value, or a .keyvalue()
// object's id, key, value.
function childAt(item: Item, i: number): Item | null {
  if (item.kind === "kv") return kvMember(item.kv, i);
  if (item.kind !== "doc") return null;
  if (item.node.kind === "array") {
    const e = item.node.elements[i];
    return e === undefined ? null : { kind: "doc", node: e };
  }
  if (item.node.kind === "object") {
    const member = item.node.members[i];
    return member === undefined ? null : { kind: "doc", node: member.value };
  }
  return null;
}

// nextKeyValue produces a .keyvalue() frame's next pair object: one per member, each the base of its
// own continuation.
function nextKeyValue(cx: Ctx, f: Frame): Out {
  if (!f.started) {
    f.started = true;
    charge(cx, 1);
    if (typeName(f.item) !== "object") {
      throw engineError(
        "sql_json_object_not_found",
        "jsonpath item method .keyvalue() can only be applied to an object",
      );
    }
    f.kvId = keyvalueId(cx, f.item, f.base);
  }
  const i = f.a;
  f.a += 1;
  let key: string;
  let value: Item;
  if (f.item.kind === "doc" && f.item.node.kind === "object") {
    const member = f.item.node.members[i];
    if (member === undefined) return DONE;
    key = member.key;
    value = { kind: "doc", node: member.value };
  } else if (f.item.kind === "kv") {
    const v = kvMember(f.item.kv, i);
    if (v === null) return DONE;
    key = KV_KEYS[i]!;
    value = v;
  } else {
    return DONE;
  }
  const kv: KvPair = { key, value, id: f.kvId, generated: cx.nextGen, tree: baseTree(cx, f.base) };
  cx.nextGen += 1n;
  charge(cx, 1);
  return { kind: "item", item: { kind: "kv", kv }, base: { kv, varTree: null, varId: 0n } };
}

// KV_KEYS is the member names of a .keyvalue() object, in canonical key order.
const KV_KEYS = ["id", "key", "value"] as const;

// kvMember is the i-th member value of a .keyvalue() object.
function kvMember(kv: KvPair, i: number): Item | null {
  switch (i) {
    case 0:
      return { kind: "val", node: { kind: "number", dec: Decimal.fromBigInt(kv.id) } };
    case 1:
      return { kind: "val", node: { kind: "string", value: kv.key } };
    case 2:
      return kv.value;
    default:
      return null;
  }
}

// memberOf is `.key` on one item (§7 rule 3): one unit per member examined (stored order, up to and
// including the first match). A missing member or a non-object raises in strict and yields nothing
// in lax.
function memberOf(cx: Ctx, item: Item, key: string): Item | null {
  let found: Item | null = null;
  if (item.kind === "doc" && item.node.kind === "object") {
    const members = item.node.members;
    const at = members.findIndex((member) => member.key === key);
    charge(cx, at < 0 ? members.length : at + 1);
    if (at >= 0) found = { kind: "doc", node: members[at]!.value };
  } else if (item.kind === "kv") {
    const at = (KV_KEYS as readonly string[]).indexOf(key);
    charge(cx, at < 0 ? KV_KEYS.length : at + 1);
    if (at >= 0) found = kvMember(item.kv, at);
  } else if (structural(cx)) {
    throw engineError(
      "sql_json_member_not_found",
      "jsonpath member accessor can only be applied to an object",
    );
  } else {
    return null;
  }
  if (found === null && structural(cx)) {
    throw engineError("sql_json_member_not_found", `JSON object does not contain key "${key}"`);
  }
  return found;
}

// subscriptRange resolves one subscript against an array of size elements → its inclusive
// [from, to] range, clamped. An index is truncated toward zero; one outside the integer range raises
// in both modes, and strict raises on an out-of-bounds or reversed range, which lax just leaves
// (partly) empty. A non-literal index (`last - 1`, `$.i`) is evaluated with `last` naming this array
// and must be a single number (22033).
function subscriptRange(
  cx: Ctx,
  sub: Subscript,
  size: number,
  current: Item,
  base: Base,
): [number, number] {
  const from = indexValue(cx, sub.kind === "index" ? sub.index : sub.from, size, current, base);
  const to = sub.kind === "index" ? from : indexValue(cx, sub.to, size, current, base);
  if (structural(cx) && (from < 0 || from > to || to >= size)) {
    throw engineError("invalid_sql_json_subscript", "jsonpath array subscript is out of bounds");
  }
  return [Math.max(from, 0), Math.min(to, size - 1)];
}

// indexValue is one subscript index's value (PostgreSQL's getArrayIndex).
function indexValue(cx: Ctx, i: Index, size: number, current: Item, base: Base): number {
  let n: Decimal;
  switch (i.kind) {
    case "last":
      return size - 1;
    case "number":
      n = i.value;
      break;
    case "expr": {
      const seq = evalSeq(cx, i.expr, current, base, size, false);
      const node = seq.length === 1 ? scalarNode(seq[0]!) : null;
      if (node === null || node.kind !== "number") {
        throw engineError(
          "invalid_sql_json_subscript",
          "jsonpath array subscript is not a single numeric value",
        );
      }
      n = node.dec;
      break;
    }
  }
  const v = n.truncPlaces(0).toBigIntRound();
  if (v === null || v < -2147483648n || v > 2147483647n) {
    throw engineError(
      "invalid_sql_json_subscript",
      "jsonpath array subscript is out of integer range",
    );
  }
  return Number(v);
}

// evalPred evaluates a filter predicate to a Kleene truth value (`true`/`false`/`null` = unknown).
// Both operands of &&/|| are always evaluated (no short-circuit), so their cost is fixed (§7).
function evalPred(
  cx: Ctx,
  pred: Pred,
  current: Item,
  base: Base,
  last: number | null,
): boolean | null {
  switch (pred.kind) {
    case "or": {
      const x = evalPred(cx, pred.a, current, base, last);
      const y = evalPred(cx, pred.b, current, base, last);
      if (x === true || y === true) return true;
      if (x === false && y === false) return false;
      return null;
    }
    case "and": {
      const x = evalPred(cx, pred.a, current, base, last);
      const y = evalPred(cx, pred.b, current, base, last);
      if (x === false || y === false) return false;
      if (x === true && y === true) return true;
      return null;
    }
    case "not": {
      const x = evalPred(cx, pred.p, current, base, last);
      return x === null ? null : !x;
    }
    case "isUnknown":
      return evalPred(cx, pred.p, current, base, last) === null;
    case "exists":
      return evalExists(cx, pred.e, current, base, last);
    case "startsWith":
      return evalStartsWith(cx, pred.lhs, pred.rhs, current, base, last);
    case "likeRegex":
      return evalLikeRegex(cx, pred, current, base, last);
    case "compare":
      return evalCompare(cx, pred.lhs, pred.op, pred.rhs, current, base, last);
  }
}

// evalExists is `exists (e)` (§4.4 rule 6): true when e's sequence is non-empty, unknown when
// evaluating it raises a SQL/JSON error. Lax stops at the first item; strict evaluates them all.
function evalExists(
  cx: Ctx,
  e: PathExpr,
  current: Item,
  base: Base,
  last: number | null,
): boolean | null {
  const seq: Item[] = [];
  try {
    runExpr(cx, e, current, base, last, !cx.strict, seq);
  } catch (err) {
    if (isSoft(err)) return null;
    throw err;
  }
  return seq.length > 0;
}

// combinePairs combines an existential predicate's n pair results in order (§4.4 rule 3): lax is
// true at the first true pair, else unknown if any pair was, else false; strict is unknown at the
// first unknown pair, else true if any pair was, else false. A pair test charges its own work.
function combinePairs(cx: Ctx, n: number, test: (k: number) => boolean | null): boolean | null {
  let found = false;
  let anyUnknown = false;
  for (let k = 0; k < n; k++) {
    const r = test(k);
    if (r === null) {
      if (cx.strict) return null;
      anyUnknown = true;
    } else if (r) {
      if (!cx.strict) return true;
      found = true;
    }
  }
  if (found) return true;
  return anyUnknown ? null : false;
}

// chargeStringCompare charges a SQL comparison's size extra for two strings: varlen_compare per code
// point of the shorter one beyond the first (§7 rule 4).
function chargeStringCompare(cx: Ctx, a: string, b: string): void {
  const w = Math.max(Math.min(codePoints(a), codePoints(b)), 1);
  cx.m.charge(COSTS.varlenCompare * BigInt(w - 1));
  cx.m.guard();
}

// evalStartsWith is `l starts with r` (§4.4 rule 5): l's items (lax-unwrapped) against r's (not
// unwrapped); a pair of strings is true when the left begins with the right, any other pair unknown.
// A UTF-16 prefix of a well-formed string is a code-point prefix, so startsWith is the spec's test.
function evalStartsWith(
  cx: Ctx,
  lhs: PathExpr,
  rhs: PathExpr,
  current: Item,
  base: Base,
  last: number | null,
): boolean | null {
  const ls = evalOperand(cx, lhs, current, base, last);
  if (ls === null) return null;
  let rs: Item[];
  try {
    rs = evalSeq(cx, rhs, current, base, last, false);
  } catch (err) {
    if (isSoft(err)) return null;
    throw err;
  }
  return combinePairs(cx, ls.length * rs.length, (k) => {
    charge(cx, 1);
    const a = scalarNode(ls[Math.floor(k / rs.length)]!);
    const b = scalarNode(rs[k % rs.length]!);
    if (a === null || b === null || a.kind !== "string" || b.kind !== "string") return null;
    chargeStringCompare(cx, a.value, b.value);
    return a.value.startsWith(b.value);
  });
}

// evalLikeRegex is `l like_regex "p"` (§4.4 rule 5): each of l's items (lax-unwrapped) is true when
// it is a string the pattern matches, false when a string it does not, unknown otherwise. The match
// charges its regex_step units (regex.md §6).
function evalLikeRegex(
  cx: Ctx,
  pred: { lhs: PathExpr; flags: number; regex: RegexProgram },
  current: Item,
  base: Base,
  last: number | null,
): boolean | null {
  const ls = evalOperand(cx, pred.lhs, current, base, last);
  if (ls === null) return null;
  return combinePairs(cx, ls.length, (k) => {
    charge(cx, 1);
    const a = scalarNode(ls[k]!);
    if (a === null || a.kind !== "string") return null;
    const subject = (pred.flags & REGEX_I) !== 0 ? foldLowerSimple(a.value, cx.prop) : a.value;
    return regexIsMatch(
      pred.regex,
      Array.from(subject, (c) => c.codePointAt(0) as number),
      cx.m,
    );
  });
}

function codePoints(s: string): number {
  let n = 0;
  for (const _ of s) n++;
  return n;
}

// evalCompare is the existential comparison (§4.4) over every pair `(a in lhs-seq, b in rhs-seq)`,
// left outer. An operand error → null (unknown); the right operand is not evaluated when the left
// one fails. lax: any true pair → true, else any incomparable pair → unknown, else false (so an
// empty operand is false). strict: any incomparable pair → unknown, even beside a true pair. Each
// tested pair charges its unit plus a SQL comparison's size extra (§7 rule 4).
function evalCompare(
  cx: Ctx,
  lhs: PathExpr,
  op: CmpOp,
  rhs: PathExpr,
  current: Item,
  base: Base,
  last: number | null,
): boolean | null {
  const ls = evalOperand(cx, lhs, current, base, last);
  if (ls === null) return null;
  const rs = evalOperand(cx, rhs, current, base, last);
  if (rs === null) return null;
  return combinePairs(cx, ls.length * rs.length, (k) => {
    charge(cx, 1);
    const x = ls[Math.floor(k / rs.length)]!;
    const y = rs[k % rs.length]!;
    const a = scalarNode(x);
    const b = scalarNode(y);
    if (a !== null && b !== null && a.kind === "string" && b.kind === "string") {
      chargeStringCompare(cx, a.value, b.value);
    } else if (a !== null && b !== null && a.kind === "number" && b.kind === "number") {
      cx.m.charge(COSTS.decimalWork * BigInt(workLinear(a.dec, b.dec) - 1));
      cx.m.guard();
    }
    return compareItems(x, op, y);
  });
}

// evalOperand evaluates a comparison operand to its item sequence, an array item unwrapped one level
// in lax (§4.1 rule 4). `null` is a SQL/JSON error, which makes the comparison unknown rather than
// propagating (§4.2: filter operands never raise). A cost, budget, or cancellation error still
// propagates (§7).
function evalOperand(
  cx: Ctx,
  e: PathExpr,
  current: Item,
  base: Base,
  last: number | null,
): Item[] | null {
  try {
    return evalSeq(cx, e, current, base, last, true);
  } catch (err) {
    if (isSoft(err)) return null;
    throw err;
  }
}

// compareItems compares two items under a jsonpath operator (§4.4). Same-type scalars compare by
// their order (numbers numerically, strings by UTF-8 byte order, false < true, null = null —
// jsonNodeCmp on same-type operands) under every operator. null against any other item is `!=` and
// nothing else. Any other pair (mixed non-null types, or two arrays/objects) is null (unknown). A
// .keyvalue() object is an object.
function compareItems(x: Item, op: CmpOp, y: Item): boolean | null {
  const a = scalarNode(x);
  const b = scalarNode(y);
  if (a === null || b === null) {
    // An object against null is `!=`; against anything else it is not comparable.
    if (a?.kind === "null" || b?.kind === "null") return op === "ne";
    return null;
  }
  let ord: number;
  if (a.kind === "null" && b.kind === "null") {
    ord = 0;
  } else if (a.kind === "null" || b.kind === "null") {
    return op === "ne";
  } else if (
    a.kind === b.kind &&
    (a.kind === "number" || a.kind === "string" || a.kind === "bool")
  ) {
    ord = jsonNodeCmp(a, b);
  } else {
    return null; // mixed types and containers are not comparable
  }
  switch (op) {
    case "eq":
      return ord === 0;
    case "ne":
      return ord !== 0;
    case "lt":
      return ord < 0;
    case "le":
      return ord <= 0;
    case "gt":
      return ord > 0;
    case "ge":
      return ord >= 0;
  }
}

// ---------------------------------------------------------------------------------------------
// Item methods (jsonpath.md §3.1) — PostgreSQL's executeItemOptUnwrapTarget method arms.
// ---------------------------------------------------------------------------------------------

// nonNumeric is 22036 non_numeric_sql_json_item — the class every method coercion failure raises.
function nonNumeric(msg: string): EngineError {
  return engineError("non_numeric_sql_json_item", msg);
}

// wrongKind is the "can only be applied to …" error of a method applied to the wrong kind of item.
function wrongKind(m: Method, what: string): EngineError {
  return nonNumeric(`jsonpath item method .${m.name}() can only be applied to ${what}`);
}

// badArgument is the "argument … is invalid for type …" error of a failed conversion.
function badArgument(m: Method, arg: string, ty: string): EngineError {
  return nonNumeric(
    `argument "${arg}" of jsonpath item method .${m.name}() is invalid for type ${ty}`,
  );
}

function nanOrInfinity(m: Method): EngineError {
  return nonNumeric(`NaN or Infinity is not allowed for jsonpath item method .${m.name}()`);
}

function numberItem(dec: Decimal): Item {
  return { kind: "val", node: { kind: "number", dec } };
}

// applyMethod applies a single-result item method (every method except .keyvalue()) to one item.
function applyMethod(m: Method, item: Item, strict: boolean): Item {
  const node = scalarNode(item);
  switch (m.name) {
    case "type":
      return { kind: "val", node: { kind: "string", value: typeName(item) } };
    case "size": {
      if (node !== null && node.kind === "array") {
        return numberItem(Decimal.fromBigInt(BigInt(node.elements.length)));
      }
      if (strict) {
        throw engineError(
          "sql_json_array_not_found",
          "jsonpath item method .size() can only be applied to an array",
        );
      }
      // lax: a non-array is a singleton array (§4.1 rule 3).
      return numberItem(Decimal.fromBigInt(1n));
    }
    case "abs":
    case "floor":
    case "ceiling": {
      if (node === null || node.kind !== "number") throw wrongKind(m, "a numeric value");
      const d = node.dec;
      return numberItem(m.name === "abs" ? d.abs() : m.name === "floor" ? d.floor() : d.ceil());
    }
    case "double": {
      // A number is only checked to be a valid double (the item itself is the result); a string is
      // read as a double and converted back the way PostgreSQL's float8_numeric does.
      if (node !== null && node.kind === "number") {
        float8In(m, node.dec.render());
        return item;
      }
      if (node !== null && node.kind === "string") {
        return numberItem(Decimal.fromFloat64DblDig(float8In(m, node.value)));
      }
      throw wrongKind(m, "a string or numeric value");
    }
    case "bigint":
    case "integer": {
      const ty = m.name === "bigint" ? "bigint" : "integer";
      const st = m.name === "bigint" ? "i64" : "i32";
      const lo = m.name === "bigint" ? -9223372036854775808n : -2147483648n;
      const hi = m.name === "bigint" ? 9223372036854775807n : 2147483647n;
      let v: bigint;
      if (node !== null && node.kind === "number") {
        // A number rounds half away from zero (numeric → int) and must fit the type.
        const r = node.dec.toBigIntRound();
        if (r === null || r < lo || r > hi) throw badArgument(m, node.dec.render(), ty);
        v = r;
      } else if (node !== null && node.kind === "string") {
        try {
          v = parseIntLiteral(node.value, st);
        } catch (e) {
          if (!(e instanceof EngineError)) throw e;
          throw badArgument(m, node.value, ty);
        }
      } else {
        throw wrongKind(m, "a string or numeric value");
      }
      return numberItem(Decimal.fromBigInt(v));
    }
    case "boolean": {
      let b: boolean;
      if (node !== null && node.kind === "bool") {
        return item;
      } else if (node !== null && node.kind === "number") {
        // A number must read as an integer (its text through int4 input): nonzero is true.
        const text = node.dec.render();
        try {
          b = parseIntLiteral(text, "i32") !== 0n;
        } catch (e) {
          if (!(e instanceof EngineError)) throw e;
          throw badArgument(m, text, "boolean");
        }
      } else if (node !== null && node.kind === "string") {
        const parsed = parseBool(node.value);
        if (parsed === null) throw badArgument(m, node.value, "boolean");
        b = parsed;
      } else {
        throw wrongKind(m, "a boolean, string, or numeric value");
      }
      return { kind: "val", node: { kind: "bool", value: b } };
    }
    case "number":
    case "decimal": {
      let num: Decimal;
      let numstr: string;
      if (node !== null && node.kind === "number") {
        num = node.dec;
        numstr = node.dec.render();
      } else if (node !== null && node.kind === "string") {
        num = numericIn(m, node.value);
        numstr = node.value;
      } else {
        throw wrongKind(m, "a string or numeric value");
      }
      if (m.name === "decimal" && m.precision !== null) {
        return numberItem(applyTypmod(m, num, numstr, m.precision, m.scale));
      }
      return node.kind === "number" ? item : numberItem(num);
    }
    case "string": {
      let s: string;
      if (node !== null && node.kind === "string") {
        return item;
      } else if (node !== null && node.kind === "number") {
        s = node.dec.render();
      } else if (node !== null && node.kind === "bool") {
        s = node.value ? "true" : "false";
      } else {
        throw wrongKind(m, "a boolean, string, numeric, or datetime value");
      }
      return { kind: "val", node: { kind: "string", value: s } };
    }
    case "keyvalue":
      throw new Error(".keyvalue() is applied by nextKeyValue");
  }
}

// float8In is PostgreSQL's float8in over a method argument: surrounding whitespace, a sign, digits
// with an optional point and exponent, or the special spellings. A result that overflows binary64,
// or a nonzero mantissa that underflows to zero, is out of range. NaN and ±Infinity are refused.
function float8In(m: Method, text: string): number {
  const t = trimLit(text);
  let v: number;
  try {
    v = parseFloatLiteral(t, "f64");
  } catch (e) {
    if (!(e instanceof EngineError)) throw e;
    throw badArgument(m, text, "double precision");
  }
  if (!Number.isFinite(v)) throw nanOrInfinity(m);
  const mantissa = t.split(/[eE]/)[0]!;
  if (v === 0 && /[1-9]/.test(mantissa)) throw badArgument(m, text, "double precision");
  return v;
}

// numericIn is PostgreSQL's numeric_in over a method argument (jed's own decimal grammar,
// grammar.md §36), refusing NaN and ±Infinity with their own message.
function numericIn(m: Method, text: string): Decimal {
  try {
    return parseDecimalLiteral(text).checkCap();
  } catch (e) {
    if (!(e instanceof EngineError)) throw e;
  }
  const word = trimLit(text).toLowerCase();
  const body = word.startsWith("+") || word.startsWith("-") ? word.slice(1) : word;
  if (word === "nan" || body === "infinity" || body === "inf") throw nanOrInfinity(m);
  throw badArgument(m, text, "numeric");
}

// applyTypmod is `.decimal(p [, s])`: round to numeric(p, s) (PostgreSQL's typmod rules — precision
// 1..1000, scale −1000..1000, rounding half away from zero) and require fewer than p − s integer
// digits.
function applyTypmod(
  m: Method,
  num: Decimal,
  numstr: string,
  p: Decimal,
  s: Decimal | null,
): Decimal {
  const asI32 = (d: Decimal): number | null => {
    const v = d.toBigIntRound();
    return v === null || v < -2147483648n || v > 2147483647n ? null : Number(v);
  };
  const precision = asI32(p);
  if (precision === null) {
    throw nonNumeric(
      "precision of jsonpath item method .decimal() is out of range for type integer",
    );
  }
  let scale = 0;
  if (s !== null) {
    const v = asI32(s);
    if (v === null) {
      throw nonNumeric("scale of jsonpath item method .decimal() is out of range for type integer");
    }
    scale = v;
  }
  if (precision < 1 || precision > MAX_PRECISION) {
    throw engineError(
      "invalid_parameter_value",
      `NUMERIC precision ${precision} must be between 1 and ${MAX_PRECISION}`,
    );
  }
  if (scale < -MAX_PRECISION || scale > MAX_PRECISION) {
    throw engineError(
      "invalid_parameter_value",
      `NUMERIC scale ${scale} must be between ${-MAX_PRECISION} and ${MAX_PRECISION}`,
    );
  }
  let rounded: Decimal;
  try {
    rounded = num.roundPlaces(scale);
  } catch (e) {
    if (!(e instanceof EngineError)) throw e;
    throw badArgument(m, numstr, "numeric");
  }
  // The digits from the point to the most significant nonzero digit must fit p − s.
  const intDigits = rounded.precision() - rounded.scale;
  if (!rounded.isZero() && intDigits > precision - scale) throw badArgument(m, numstr, "numeric");
  return rounded;
}

// parseBool is PostgreSQL's parse_bool: case-insensitive true/false/yes/no and their prefixes, on,
// off (or of), 1, 0 — with no surrounding whitespace. `null` when unrecognized.
function parseBool(s: string): boolean | null {
  const lower = s.toLowerCase();
  const prefixOf = (word: string): boolean => lower !== "" && word.startsWith(lower);
  switch (lower[0]) {
    case "t":
      return prefixOf("true") ? true : null;
    case "f":
      return prefixOf("false") ? false : null;
    case "y":
      return prefixOf("yes") ? true : null;
    case "n":
      return prefixOf("no") ? false : null;
    case "o":
      if (lower === "on") return true;
      return lower.length >= 2 && prefixOf("off") ? false : null;
    case "1":
      return lower.length === 1 ? true : null;
    case "0":
      return lower.length === 1 ? false : null;
    default:
      return null;
  }
}

// ---------------------------------------------------------------------------------------------
// .keyvalue() ids — byte offsets in PostgreSQL's binary jsonb layout (jsonpath.md §3.1).
// ---------------------------------------------------------------------------------------------

const BASE_STRIDE = 10_000_000_000n;

// keyvalueId is the id of a .keyvalue() object: 10^10 × base number + the object's byte offset
// inside the base object, where the base is the context item (number 0), a variable's base object
// (§5), or the .keyvalue() object whose continuation is running. PostgreSQL measures the offset in
// its binary jsonb layout, which is a pure function of the value, so jed computes the same layout
// arithmetically.
function keyvalueId(cx: Ctx, item: Item, base: Base): bigint {
  const kv = base.kv;
  if (kv === null) {
    const tree = base.varTree ?? cx.root;
    const offset = item.kind === "doc" ? BigInt(treeOffset(cx, tree, item.node)) : 0n;
    return base.varId * BASE_STRIDE + offset;
  }
  let offset = 0;
  if (item.kind === "doc" && kv.value.kind === "doc") {
    // A container inside the pair's value: the value's offset in the pair object plus the
    // container's offset inside the value (both are 4-byte aligned, so the value's internal layout
    // does not depend on where it sits).
    const valueAt = align4(40 + pgNumericSize(Decimal.fromBigInt(kv.id)) + utf8Length(kv.key));
    offset = valueAt + treeOffset(cx, kv.tree, item.node) - treeOffset(cx, kv.tree, kv.value.node);
  }
  return kv.generated * BASE_STRIDE + BigInt(offset);
}

// baseTree is the base object whose binary layout a container under `base` is measured in: the
// document, a variable's base object, or the tree of the .keyvalue() pair's value.
function baseTree(cx: Ctx, base: Base): JsonNode {
  if (base.kv !== null) return base.kv.tree;
  return base.varTree ?? cx.root;
}

// treeOffset is a container's offset in the binary layout of the base object `tree` that holds it,
// laying the base object out first if no .keyvalue() has yet.
function treeOffset(cx: Ctx, tree: JsonNode, n: JsonNode): number {
  let offsets = cx.offsets.get(tree);
  if (offsets === undefined) {
    offsets = new Map();
    pgLayout(tree, 0, offsets);
    cx.offsets.set(tree, offsets);
  }
  return offsets.get(n) ?? 0;
}

// variable is the value of the path variable `$name` with the base object its continuation runs
// under (PostgreSQL's getJsonPathVariable): a member of the vars object, found by a binary search of
// its canonical key order, or the first PASSING binding of that name. A name with no binding is
// 42704, which neither lax mode, silent mode, nor a comparison operand suppresses.
function variable(cx: Ctx, name: string): { item: Item; base: Base } {
  const vars = cx.vars;
  if (vars.kind === "object" && vars.node.kind === "object") {
    const members = vars.node.members;
    let lo = 0;
    let hi = members.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      const c = keyCmp(members[mid]!.key, name);
      if (c === 0) {
        return {
          item: { kind: "doc", node: members[mid]!.value },
          base: { kv: null, varTree: vars.node, varId: 1n },
        };
      }
      if (c < 0) lo = mid + 1;
      else hi = mid;
    }
  } else if (vars.kind === "passing") {
    let lo = 0;
    let hi = vars.vars.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      const v = vars.vars[mid]!;
      const c = compareTextC(v.name, name);
      if (c === 0) {
        return {
          item: { kind: "doc", node: v.value },
          base: { kv: null, varTree: v.value, varId: v.id },
        };
      }
      if (c < 0) lo = mid + 1;
      else hi = mid;
    }
  }
  throw engineError("undefined_object", `could not find jsonpath variable "${name}"`);
}

function align4(off: number): number {
  return Math.ceil(off / 4) * 4;
}

// pgLayout lays n out at byte offset off the way PostgreSQL's convertJsonbValue does, recording each
// container's start, and returns the end offset. A container is 4-byte aligned: a header word, one
// 4-byte entry per element (two per object member — keys, then values), then the keys' bytes and the
// element/value data. A string is its UTF-8 bytes, a number its aligned numeric datum, and
// null/true/false take no data bytes.
function pgLayout(n: JsonNode, off: number, offsets: Map<JsonNode, number>): number {
  switch (n.kind) {
    case "null":
    case "bool":
      return off;
    case "string":
      return off + utf8Length(n.value);
    case "number":
      return align4(off) + pgNumericSize(n.dec);
    case "array": {
      const start = align4(off);
      offsets.set(n, start);
      let p = start + 4 + 4 * n.elements.length;
      for (const e of n.elements) p = pgLayout(e, p, offsets);
      return p;
    }
    case "object": {
      const start = align4(off);
      offsets.set(n, start);
      let p = start + 4 + 8 * n.members.length;
      for (const member of n.members) p += utf8Length(member.key);
      for (const member of n.members) p = pgLayout(member.value, p, offsets);
      return p;
    }
  }
}

// pgNumericSize is the byte size of PostgreSQL's numeric datum for d: a 4-byte varlena header, a
// 2-byte header (short form) or 4 bytes (long form), and one 2-byte base-10000 digit per group from
// the most to the least significant nonzero group. The short form needs a display scale ≤ 63 and a
// weight (the leading group's power of 10000) in −64..=63. Zero has no digits.
function pgNumericSize(d: Decimal): number {
  const text = d.render().replace(/^-/, "");
  const dot = text.indexOf(".");
  const intPart = dot < 0 ? text : text.slice(0, dot);
  const frac = dot < 0 ? "" : text.slice(dot + 1);
  const digits = intPart + frac;
  const first = digits.search(/[1-9]/);
  if (first < 0) return 6;
  let last = digits.length - 1;
  while (digits[last] === "0") last--;
  const power = (i: number): number => intPart.length - 1 - i;
  const weight = Math.floor(power(first) / 4);
  const ndigits = weight - Math.floor(power(last) / 4) + 1;
  const short = d.scale <= 63 && weight >= -64 && weight <= 63;
  return (short ? 6 : 8) + 2 * ndigits;
}
