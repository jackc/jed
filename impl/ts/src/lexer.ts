// Hand-written lexer (CLAUDE.md §5: parsers are per-language, not codegen'd). Produces
// tokens terminated by an "eof" token. Integer literals may carry a leading '-' and are
// parsed to bigint; a value outside the i64 range is a structured 42601 syntax error
// (matching Go's strconv.ParseInt(.., 64) behaviour). Errors throw EngineError.

import { decimalFromParts, EXP_LIMIT } from "./decimal.ts";
import { EngineError, engineError } from "./errors.ts";
import { utf8Length } from "./memsize.ts";
import type { Token } from "./token.ts";

// The maximum integer-literal MAGNITUDE the lexer accepts: 2^63, so that the unary
// minus of it folds to i64's minimum. A larger magnitude cannot be represented.
const MAX_MAGNITUDE = 9223372036854775808n;

// MAX_IDENTIFIER_LENGTH is the maximum length, in bytes, of a single identifier — table / column /
// type / alias / function name (spec/design/cost.md §7; CLAUDE.md §13). The §13 identifier
// hardening gate for untrusted input: an unbounded identifier would otherwise consume O(input)
// memory and land verbatim in the on-disk catalog and keys. Checked below when an identifier token
// is built (the producer, so every identifier on every parse path is bounded), throwing 42622
// name_too_long. Identifiers are ASCII-only (spec/design/grammar.md §3), so a substring's .length
// here equals its UTF-8 byte length. 63 matches PostgreSQL's NAMEDATALEN − 1 boundary — but jed
// throws where PG silently truncates (a documented PG divergence: jed has no notices, and a silent
// truncation could collide two distinct names — CLAUDE.md §1). A fixed constant, so it is
// deterministic and cross-core identical (§8): the SAME in every core (Rust / Go / TS).
export const MAX_IDENTIFIER_LENGTH = 63;

function isDigit(c: string): boolean {
  return c >= "0" && c <= "9";
}

function isAlpha(c: string): boolean {
  return c === "_" || (c >= "a" && c <= "z") || (c >= "A" && c <= "Z");
}

// scanExponent: if sql[i..] begins a well-formed exponent [eE][+-]?digit+, return [exp, nextIndex]
// with the magnitude clamped to ±EXP_LIMIT. Otherwise [null, i] — a bare e / ex is NOT part of the
// number (it lexes as the following token, exactly as before e-notation existed; PostgreSQL
// likewise rejects `1e` as trailing junk rather than reading it as the number `1`).
function scanExponent(sql: string, i: number): [number | null, number] {
  const n = sql.length;
  if (i >= n || (sql[i] !== "e" && sql[i] !== "E")) return [null, i];
  let j = i + 1;
  let neg = false;
  if (j < n && (sql[j] === "+" || sql[j] === "-")) {
    neg = sql[j] === "-";
    j++;
  }
  if (j >= n || !isDigit(sql[j]!)) return [null, i];
  let exp = 0;
  while (j < n && isDigit(sql[j]!)) {
    if (exp < EXP_LIMIT) {
      exp = exp * 10 + (sql.charCodeAt(j) - 48);
      if (exp > EXP_LIMIT) exp = EXP_LIMIT;
    }
    j++;
  }
  return [neg ? -exp : exp, j];
}

// lex tokenizes sql into tokens terminated by an "eof" token.
//
// This is the stored-text entry point (a view body, a CHECK expression, a rename rewrite), so an
// error carries no position: it would point into text the host never wrote
// (spec/design/error-fields.md §10.1). lexRaw never sets one; only lexWithPositions stamps it.
export function lex(sql: string): Token[] {
  return lexRaw(sql, { starts: [], tokStart: 0 });
}

// lexWithPositions is lex, also returning each token's 1-based CHARACTER position in sql
// (spec/design/error-fields.md §10.1), index-aligned with the tokens. The eof token's position is
// one past the last character — PostgreSQL's "at end of input". An error carries the position of
// the lexeme being scanned (spec/design/error-fields.md §10.2).
export function lexWithPositions(sql: string): { tokens: Token[]; positions: number[] } {
  const st: LexState = { starts: [], tokStart: 0 };
  let tokens: Token[];
  try {
    tokens = lexRaw(sql, st);
  } catch (e) {
    if (e instanceof EngineError) e.withPosition(charPositions(sql, [st.tokStart])[0]);
    throw e;
  }
  return { tokens, positions: charPositions(sql, st.starts) };
}

// charPositions converts ascending UTF-16 offsets into sql to 1-based character positions in one
// pass. A character is a code point, so a surrogate pair counts once: count every code unit except
// the low half of a pair.
export function charPositions(sql: string, starts: number[]): number[] {
  const out: number[] = [];
  let b = 0;
  let chars = 0;
  for (const s of starts) {
    while (b < s) {
      if (!isPairLowHalf(sql, b)) chars++;
      b++;
    }
    out.push(chars + 1);
  }
  return out;
}

// charCount is the number of characters (code points) in sql's UTF-16 range [from, to): a script
// statement's error position is shifted by the characters before its span (error-fields.md §10.1).
export function charCount(sql: string, from: number, to: number): number {
  let chars = 0;
  for (let b = from; b < to; b++) if (!isPairLowHalf(sql, b)) chars++;
  return chars;
}

// isPairLowHalf reports whether the code unit at i is the low surrogate of a surrogate pair (the
// second code unit of a code point that charPositions has already counted).
function isPairLowHalf(sql: string, i: number): boolean {
  const u = sql.charCodeAt(i);
  if (u < 0xdc00 || u > 0xdfff || i === 0) return false;
  const prev = sql.charCodeAt(i - 1);
  return prev >= 0xd800 && prev <= 0xdbff;
}

// LexState is lexRaw's position bookkeeping: each token's start UTF-16 offset (index-aligned with
// the tokens), and the offset where the lexeme being scanned began, so a caller can position an
// error.
type LexState = { starts: number[]; tokStart: number };

// lexRaw is the lexer proper.
function lexRaw(sql: string, st: LexState): Token[] {
  const tokens: Token[] = [];
  // Indices of the `U&'…'` / `U&"…"` tokens, decoded after the scan (applyUnicodeEscapes).
  const unicodeEscapes: number[] = [];
  let i = 0;
  const n = sql.length;

  while (i < n) {
    // The previous iteration's tokens started at its tokStart; this one's start at i.
    while (st.starts.length < tokens.length) st.starts.push(st.tokStart);
    st.tokStart = i;
    const c = sql[i]!;
    if (c === " " || c === "\t" || c === "\r" || c === "\n") {
      i++;
    } else if (c === ",") {
      tokens.push({ kind: "comma" });
      i++;
    } else if (c === "(") {
      tokens.push({ kind: "lparen" });
      i++;
    } else if (c === ")") {
      tokens.push({ kind: "rparen" });
      i++;
    } else if (c === "[") {
      tokens.push({ kind: "lbracket" });
      i++;
    } else if (c === "]") {
      tokens.push({ kind: "rbracket" });
      i++;
    } else if (c === "*") {
      tokens.push({ kind: "star" });
      i++;
    } else if (c === "+") {
      tokens.push({ kind: "plus" });
      i++;
    } else if (c === "-") {
      // `-|-` is the range adjacency operator (range-functions.md §3), scanned greedily and
      // checked FIRST so it is never mistaken for `-` (minus) `|-`. Its middle `|` keeps it
      // disjoint from the `--` line comment (which needs a second `-`).
      if (i + 2 < n && sql[i + 1] === "|" && sql[i + 2] === "-") {
        tokens.push({ kind: "adjacent" });
        i += 3;
      } else if (i + 2 < n && sql[i + 1] === ">" && sql[i + 2] === ">") {
        // `->>` is the jsonb accessor-as-text operator (json-sql-functions.md §1), scanned
        // greedily BEFORE `->` so it is never `-> >`.
        tokens.push({ kind: "arrowText" });
        i += 3;
      } else if (i + 1 < n && sql[i + 1] === ">") {
        // `->` is the jsonb accessor operator (json-sql-functions.md §1).
        tokens.push({ kind: "arrow" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "-") {
        // `--` starts a line comment running to the end of the line; comments are
        // whitespace (grammar.md §33). Two hyphens ALWAYS start a comment outside a
        // string, even abutting a token (`1--2` is `1` — PG behavior).
        i += 2;
        while (i < n && sql[i] !== "\n" && sql[i] !== "\r") {
          i++;
        }
      } else {
        tokens.push({ kind: "minus" });
        i++;
      }
    } else if (c === "/") {
      // `/*` starts a block comment; blocks NEST (PG / the SQL standard), so a depth
      // counter tracks open/close pairs. End of input at depth >= 1 is 42601
      // (grammar.md §33). A `*/` with no opener is NOT comment syntax — it lexes as
      // `*` `/` and fails at parse.
      if (i + 1 < n && sql[i + 1] === "*") {
        i += 2;
        let depth = 1;
        while (depth > 0) {
          if (i + 1 >= n) {
            throw engineError("syntax_error", "unterminated /* comment");
          }
          if (sql[i] === "/" && sql[i + 1] === "*") {
            depth++;
            i += 2;
          } else if (sql[i] === "*" && sql[i + 1] === "/") {
            depth--;
            i += 2;
          } else {
            i++;
          }
        }
      } else {
        tokens.push({ kind: "slash" });
        i++;
      }
    } else if (c === "%") {
      tokens.push({ kind: "percent" });
      i++;
    } else if (c === "|") {
      // `||` is the array concatenation operator (grammar.md §39), scanned greedily as one token;
      // a lone `|` is not part of jed's surface (no bitwise-or) — 42601.
      if (i + 1 < n && sql[i + 1] === "|") {
        tokens.push({ kind: "concat" });
        i += 2;
      } else {
        throw engineError("syntax_error", "unexpected character '|'");
      }
    } else if (c === ":") {
      // `::` is the PostgreSQL typecast operator (grammar.md §37), scanned greedily as one
      // token; a lone `:` is the array-slice separator a[m:n] (array.md §6).
      if (i + 1 < n && sql[i + 1] === ":") {
        tokens.push({ kind: "doubleColon" });
        i += 2;
      } else {
        tokens.push({ kind: "colon" });
        i++;
      }
    } else if (c === "=") {
      // `=>` is the named-argument arrow (grammar.md §17), scanned greedily as one token;
      // a bare `=` is the equality operator.
      if (i + 1 < n && sql[i + 1] === ">") {
        tokens.push({ kind: "fatArrow" });
        i += 2;
      } else {
        tokens.push({ kind: "eq" });
        i++;
      }
    } else if (c === "<") {
      if (i + 1 < n && sql[i + 1] === "=") {
        tokens.push({ kind: "le" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === ">") {
        // `<>` is the not-equal operator (grammar.md §4), scanned greedily; its `!=` alias is
        // handled in the `!` branch and folds to the same token.
        tokens.push({ kind: "ne" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "@") {
        // `<@` is the array contained-by operator (grammar.md §40), scanned greedily.
        tokens.push({ kind: "containedBy" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "<") {
        // `<<` is the range strictly-left operator (range-functions.md §3), scanned greedily.
        tokens.push({ kind: "strictlyLeft" });
        i += 2;
      } else {
        tokens.push({ kind: "lt" });
        i++;
      }
    } else if (c === "!") {
      // `!~*` / `!~` are the negated regex match operators (grammar.md §22b), checked greedily
      // BEFORE the `!=`→`ne` alias and the lone-`!` error. `!=` is the PostgreSQL alias for `<>`.
      // A lone `!` is not part of jed's surface (no factorial / boolean-not) — 42601.
      if (i + 2 < n && sql[i + 1] === "~" && sql[i + 2] === "*") {
        tokens.push({ kind: "bangTildeStar" });
        i += 3;
      } else if (i + 1 < n && sql[i + 1] === "~") {
        tokens.push({ kind: "bangTilde" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "=") {
        tokens.push({ kind: "ne" });
        i += 2;
      } else {
        throw engineError("syntax_error", "unexpected character '!'");
      }
    } else if (c === "~") {
      // `~*` is the case-insensitive regex match operator (grammar.md §22b), scanned greedily as
      // one token (never `~` `star`); a bare `~` is the case-sensitive form.
      if (i + 1 < n && sql[i + 1] === "*") {
        tokens.push({ kind: "tildeStar" });
        i += 2;
      } else {
        tokens.push({ kind: "tilde" });
        i += 1;
      }
    } else if (c === "@") {
      // `@>` array containment (grammar.md §40); `@?` / `@@` the jsonpath exists / match operators
      // (jsonpath.md §6). Scanned greedily; a lone `@` is not part of jed's surface — 42601.
      if (i + 1 < n && sql[i + 1] === ">") {
        tokens.push({ kind: "contains" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "?") {
        tokens.push({ kind: "jsonPathExists" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "@") {
        tokens.push({ kind: "jsonPathMatch" });
        i += 2;
      } else {
        throw engineError("syntax_error", "unexpected character '@'");
      }
    } else if (c === "#") {
      // `#>>` / `#>` (get-at-path) and `#-` (delete-at-path) are the jsonb path operators
      // (json-sql-functions.md §1), scanned greedily (`#>>` before `#>`). A lone `#` is not
      // part of jed's surface — 42601.
      if (i + 2 < n && sql[i + 1] === ">" && sql[i + 2] === ">") {
        tokens.push({ kind: "hashArrowText" });
        i += 3;
      } else if (i + 1 < n && sql[i + 1] === ">") {
        tokens.push({ kind: "hashArrow" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "-") {
        tokens.push({ kind: "hashMinus" });
        i += 2;
      } else {
        throw engineError("syntax_error", "unexpected character '#'");
      }
    } else if (c === "?") {
      // `?|` (any-key) / `?&` (all-keys) / `?` (key-exists) are the jsonb key-existence operators
      // (json-sql-functions.md §1), scanned greedily. A lone `?` is the key-exists operator (jed
      // binds parameters as `$N`, never `?`).
      if (i + 1 < n && sql[i + 1] === "|") {
        tokens.push({ kind: "questionPipe" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "&") {
        tokens.push({ kind: "questionAmp" });
        i += 2;
      } else {
        tokens.push({ kind: "question" });
        i += 1;
      }
    } else if (c === "&") {
      // `&&` is the array overlap operator (grammar.md §40); `&<` (not-extend-right) and `&>`
      // (not-extend-left) are the range positional operators (range-functions.md §3). Each
      // scanned greedily; a lone `&` is not part of jed's surface (no bitwise-and) — 42601.
      if (i + 1 < n && sql[i + 1] === "&") {
        tokens.push({ kind: "overlaps" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === "<") {
        tokens.push({ kind: "notExtendRight" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === ">") {
        tokens.push({ kind: "notExtendLeft" });
        i += 2;
      } else {
        throw engineError("syntax_error", "unexpected character '&'");
      }
    } else if (c === ">") {
      if (i + 1 < n && sql[i + 1] === "=") {
        tokens.push({ kind: "ge" });
        i += 2;
      } else if (i + 1 < n && sql[i + 1] === ">") {
        // `>>` is the range strictly-right operator (range-functions.md §3), scanned greedily.
        tokens.push({ kind: "strictlyRight" });
        i += 2;
      } else {
        tokens.push({ kind: "gt" });
        i++;
      }
    } else if (c === "'") {
      // Single-quoted string literal (the text type). `''` is an embedded single quote;
      // backslash is an ordinary character (no C-style escapes — standard_conforming_strings,
      // spec/design/types.md §11). Accumulating code units verbatim preserves the string
      // (a surrogate pair's halves rejoin), so multibyte/astral text round-trips.
      i++; // consume the opening quote
      const [s, end] = scanQuotedBody(sql, i, "'");
      i = end;
      if (s === null) {
        throw engineError("syntax_error", "unterminated string literal");
      }
      tokens.push({ kind: "str", str: s });
    } else if (c === '"') {
      // Double-quoted identifier (spec/design/grammar.md §3): the name VERBATIM (case-sensitive,
      // any UTF-8), `""` an embedded double quote. Accumulating code units verbatim keeps a
      // surrogate pair's halves together.
      i++; // consume the opening quote
      const [s, end] = scanQuotedBody(sql, i, '"');
      i = end;
      if (s === null) {
        throw engineError("syntax_error", "unterminated quoted identifier");
      }
      // A zero-length delimited identifier is a syntax error (PostgreSQL's rule,
      // spec/design/grammar.md §3).
      if (s === "") {
        throw engineError("syntax_error", "zero-length delimited identifier");
      }
      // The identifier-length gate applies to a quoted name too, measured on its decoded UTF-8
      // bytes (spec/design/cost.md §7) — NOT its UTF-16 .length.
      if (utf8Length(s) > MAX_IDENTIFIER_LENGTH) {
        throw nameTooLong();
      }
      tokens.push({ kind: "quotedIdent", str: s });
    } else if (
      (c === "u" || c === "U") &&
      sql[i + 1] === "&" &&
      (sql[i + 2] === "'" || sql[i + 2] === '"')
    ) {
      // A Unicode-escape string `U&'…'` or identifier `U&"…"` (grammar.md §3). The body is scanned
      // exactly like its plain form (`''` / `""` doubled) and kept RAW here; applyUnicodeEscapes
      // decodes it once the whole input is lexed, because an optional `UESCAPE '<c>'` after it
      // chooses the escape character.
      const quote = sql[i + 2]!;
      i += 3; // consume `U&` and the opening quote
      const [s, end] = scanQuotedBody(sql, i, quote);
      i = end;
      if (s === null) {
        throw engineError(
          "syntax_error",
          quote === "'" ? "unterminated string literal" : "unterminated quoted identifier",
        );
      }
      // The zero-length check precedes decoding (PostgreSQL's order); every escape decodes to at
      // least one character, so a non-empty body never decodes empty.
      if (quote === '"' && s === "") {
        throw engineError("syntax_error", "zero-length delimited identifier");
      }
      unicodeEscapes.push(tokens.length);
      tokens.push(quote === "'" ? { kind: "str", str: s } : { kind: "quotedIdent", str: s });
    } else if (isDigit(c)) {
      // A numeric literal. Scan the integer digits; a following "." and/or scientific
      // e-notation (`123.45`, `5e2`, `1.5e-3`) makes it a DECIMAL literal, otherwise an
      // INTEGER literal.
      const start = i;
      while (i < n && isDigit(sql[i]!)) {
        i++;
      }
      const intPart = sql.slice(start, i);
      // Optional fractional part: `123.`, `123.45`. The fractional part may be empty.
      let frac = "";
      let hasFrac = false;
      if (i < n && sql[i] === ".") {
        hasFrac = true;
        i++; // consume "."
        const fracStart = i;
        while (i < n && isDigit(sql[i]!)) {
          i++;
        }
        frac = sql.slice(fracStart, i);
      }
      // Optional exponent (`e3`, `E+2`, `e-10`); a well-formed exponent (even with no ".")
      // makes the literal a decimal.
      const [exp, next] = scanExponent(sql, i);
      i = next;
      if (hasFrac || exp !== null) {
        const [digits, scale] = decimalFromParts(intPart, frac, exp);
        tokens.push({ kind: "decimal", decDigits: digits, decScale: scale });
      } else {
        // Integer literal: an unsigned magnitude (the sign is the "minus" operator). The
        // magnitude must be <= 2^63 so that -(2^63) = i64's minimum is reachable; anything
        // larger cannot be represented (42601).
        const text = intPart;
        const v = BigInt(text);
        if (v > MAX_MAGNITUDE) {
          throw engineError("syntax_error", `integer literal out of range: ${text}`);
        }
        tokens.push({ kind: "int", int: v });
      }
    } else if (c === ".") {
      // A "." has two roles, disambiguated on the FOLLOWING char alone (no preceding-token
      // context, so the rule is trivially identical across cores — grammar.md §4): a digit
      // immediately after starts a leading-dot decimal literal (`.5`); otherwise it is the
      // "dot" token of a qualified column reference (`t.col`, §15). The lone overlap — an
      // identifier then `.<digit>` (`t.5`) — is invalid either way and lexes as a decimal,
      // rejected at parse.
      if (i + 1 < n && isDigit(sql[i + 1]!)) {
        i++; // consume "."
        const fracStart = i;
        while (i < n && isDigit(sql[i]!)) {
          i++;
        }
        const frac = sql.slice(fracStart, i);
        // A leading-dot decimal may also carry an exponent (`.5e2`).
        const [exp, next] = scanExponent(sql, i);
        i = next;
        const [digits, scale] = decimalFromParts("", frac, exp);
        tokens.push({ kind: "decimal", decDigits: digits, decScale: scale });
      } else {
        tokens.push({ kind: "dot" });
        i++;
      }
    } else if (c === "$") {
      // A bind parameter $N — "$" then a 1-based decimal index (spec/design/api.md §5,
      // grammar.md §5). "$" with no following digit, $0, and a leading zero ($01) are 42601;
      // an index overflowing a 32-bit range is too.
      i++; // consume "$"
      const digitStart = i;
      while (i < n && isDigit(sql[i]!)) {
        i++;
      }
      const digits = sql.slice(digitStart, i);
      if (digits.length === 0) {
        throw engineError("syntax_error", "expected a parameter number after '$'");
      }
      if (digits[0] === "0") {
        throw engineError(
          "syntax_error",
          `invalid parameter number $${digits}: parameters are 1-based with no leading zero`,
        );
      }
      const idx = Number(digits);
      if (!Number.isSafeInteger(idx) || idx > 0xffffffff) {
        throw engineError("syntax_error", `parameter number out of range: $${digits}`);
      }
      tokens.push({ kind: "param", paramIndex: idx });
    } else if (isAlpha(c)) {
      const start = i;
      while (i < n && (isAlpha(sql[i]!) || isDigit(sql[i]!))) {
        i++;
      }
      // Identifier-length gate (CLAUDE.md §13; spec/design/cost.md §7). A word is an identifier or
      // a keyword; identifiers are ASCII-only here (so .length = bytes), and no keyword is this
      // long, so bounding the word length bounds every identifier on every parse path. Throws
      // 42622 before the (possibly huge) name is interned.
      if (i - start > MAX_IDENTIFIER_LENGTH) {
        throw nameTooLong();
      }
      // A bare word folds to lowercase (ASCII — the word is ASCII-only) here, so every later
      // comparison of a name is byte-exact and keyword matching is case-insensitive by
      // construction (spec/design/grammar.md §3).
      tokens.push({ kind: "word", word: sql.slice(start, i).toLowerCase() });
    } else {
      throw engineError("syntax_error", `unexpected character '${c}'`);
    }
  }

  while (st.starts.length < tokens.length) st.starts.push(st.tokStart);
  st.tokStart = n;
  tokens.push({ kind: "eof" });
  st.starts.push(n);
  if (unicodeEscapes.length > 0) return applyUnicodeEscapes(sql, tokens, st, unicodeEscapes);
  return tokens;
}

// scanQuotedBody scans a quoted body from sql[i..] (just past the opening quote) through its closing
// quote, collapsing each doubled quote to one. Returns [body, nextIndex]; body is null when the
// input ends first.
function scanQuotedBody(sql: string, i: number, quote: string): [string | null, number] {
  const n = sql.length;
  let s = "";
  while (i < n) {
    if (sql[i] === quote) {
      if (i + 1 < n && sql[i + 1] === quote) {
        s += quote;
        i += 2;
        continue;
      }
      return [s, i + 1]; // consume the closing quote
    }
    s += sql[i]!;
    i++;
  }
  return [null, i];
}

// applyUnicodeEscapes decodes every `U&'…'` / `U&"…"` token (their indices in uesc, ascending) —
// PostgreSQL's base_yylex filter and str_udeescape (grammar.md §3). A `UESCAPE '<c>'` right after
// the token picks the escape character and is consumed with it; otherwise the escape is `\`. The
// decoded token is a plain str / quotedIdent, so nothing downstream (and no stored text, which
// renders it with plain quotes) ever sees the Unicode-escape spelling. An error sets st.tokStart to
// the character PostgreSQL's error cursor names.
function applyUnicodeEscapes(sql: string, tokens: Token[], st: LexState, uesc: number[]): Token[] {
  const isUesc = new Set(uesc);
  const out: Token[] = [];
  const outStarts: number[] = [];
  let idx = 0;
  while (idx < tokens.length) {
    const tok = tokens[idx]!;
    if (!isUesc.has(idx)) {
      out.push(tok);
      outStarts.push(st.starts[idx]!);
      idx++;
      continue;
    }
    const start = st.starts[idx]!;
    let escapeChar = "\\";
    let next = idx + 1;
    const after = tokens[idx + 1]!;
    if (after.kind === "word" && after.word === "uescape") {
      // The third token must be a simple string literal of exactly one byte that is not a hex
      // digit, `+`, `'`, `"`, or whitespace. Errors point at that third token. The eof token always
      // follows, so idx + 2 is in range.
      const third = idx + 2;
      st.tokStart = st.starts[third]!;
      const t3 = tokens[third]!;
      if (t3.kind !== "str" || isUesc.has(third)) {
        throw engineError("syntax_error", "UESCAPE must be followed by a simple string literal");
      }
      const esc = t3.str!;
      if (esc.length !== 1 || esc.charCodeAt(0) >= 0x80 || !isUescapeChar(esc)) {
        throw engineError("syntax_error", "invalid Unicode escape character");
      }
      escapeChar = esc;
      next = idx + 3;
    }
    const isIdent = tok.kind === "quotedIdent";
    const raw = tok.str!;
    let decoded: string;
    try {
      decoded = udeescape(raw, escapeChar);
    } catch (e) {
      if (e instanceof UnicodeEscapeError) {
        // PostgreSQL's cursor is the token start + 3 (`U&'`) + the offset into the body AFTER `''`
        // / `""` collapsing, measured in UTF-8 bytes; name the character holding that byte.
        st.tokStart = utf16AtUtf8Offset(sql, start + 3, utf8Length(raw.slice(0, e.at)));
        throw e.error;
      }
      throw e;
    }
    if (isIdent && utf8Length(decoded) > MAX_IDENTIFIER_LENGTH) {
      st.tokStart = start;
      throw nameTooLong();
    }
    out.push(isIdent ? { kind: "quotedIdent", str: decoded } : { kind: "str", str: decoded });
    outStarts.push(start);
    idx = next;
  }
  st.starts = outStarts;
  return out;
}

// utf16AtUtf8Offset is the UTF-16 index in sql of the first code point at or after `bytes` UTF-8
// bytes past `from` — so a byte offset inside a multibyte character names the next character, as
// PostgreSQL's byte-offset error cursor does.
function utf16AtUtf8Offset(sql: string, from: number, bytes: number): number {
  let u = from;
  let acc = 0;
  while (u < sql.length && acc < bytes) {
    const cp = sql.codePointAt(u)!;
    acc += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    u += cp > 0xffff ? 2 : 1;
  }
  return u;
}

// isUescapeChar is PostgreSQL's check_uescapechar: a UESCAPE character may not be a hex digit, `+`,
// `'`, `"`, or scanner whitespace.
function isUescapeChar(c: string): boolean {
  return !(isHexDigit(c) || "+'\" \t\n\r\v\f".includes(c));
}

function isHexDigit(c: string | undefined): boolean {
  return c !== undefined && /^[0-9A-Fa-f]$/.test(c);
}

// hexDigits is the value of n hex digits at raw[at..], or -1 if any is missing or not a hex digit.
function hexDigits(raw: string, at: number, n: number): number {
  let v = 0;
  for (let k = at; k < at + n; k++) {
    const c = raw[k];
    if (!isHexDigit(c)) return -1;
    v = v * 16 + parseInt(c!, 16);
  }
  return v;
}

// UnicodeEscapeError carries a udeescape failure and the code-unit index into the body PostgreSQL's
// cursor names (the escape, or the end of the body after a dangling high surrogate).
class UnicodeEscapeError extends Error {
  error: EngineError;
  at: number;
  constructor(error: EngineError, at: number) {
    super(error.message);
    this.error = error;
    this.at = at;
  }
}

// udeescape is PostgreSQL's str_udeescape: replace `<esc>XXXX` / `<esc>+XXXXXX` with the code
// point and `<esc><esc>` with `<esc>`, combining a UTF-16 surrogate pair written as two escapes.
function udeescape(raw: string, esc: string): string {
  const invalidPair = (at: number) =>
    new UnicodeEscapeError(engineError("syntax_error", "invalid Unicode surrogate pair"), at);
  let out = "";
  let pairFirst = 0;
  let i = 0;
  while (i < raw.length) {
    if (raw[i] !== esc) {
      if (pairFirst !== 0) throw invalidPair(i);
      out += raw[i];
      i++;
      continue;
    }
    if (raw[i + 1] === esc) {
      if (pairFirst !== 0) throw invalidPair(i);
      out += esc;
      i += 2;
      continue;
    }
    let cp: number;
    let len: number;
    const v4 = hexDigits(raw, i + 1, 4);
    const v6 = raw[i + 1] === "+" ? hexDigits(raw, i + 2, 6) : -1;
    if (v4 >= 0) {
      cp = v4;
      len = 5;
    } else if (v6 >= 0) {
      cp = v6;
      len = 8;
    } else {
      throw new UnicodeEscapeError(
        engineError("syntax_error", "invalid Unicode escape").withHint(
          "Unicode escapes must be \\XXXX or \\+XXXXXX.",
        ),
        i,
      );
    }
    if (cp === 0 || cp > 0x10ffff) {
      throw new UnicodeEscapeError(engineError("syntax_error", "invalid Unicode escape value"), i);
    }
    const isHigh = cp >= 0xd800 && cp <= 0xdbff;
    const isLow = cp >= 0xdc00 && cp <= 0xdfff;
    if (pairFirst !== 0) {
      if (!isLow) throw invalidPair(i);
      cp = (pairFirst & 0x3ff) * 0x400 + (cp & 0x3ff) + 0x10000;
      pairFirst = 0;
    } else if (isLow) {
      throw invalidPair(i);
    } else if (isHigh) {
      pairFirst = cp;
      i += len;
      continue;
    }
    out += String.fromCodePoint(cp);
    i += len;
  }
  if (pairFirst !== 0) throw invalidPair(raw.length);
  return out;
}

function nameTooLong(): EngineError {
  return engineError(
    "name_too_long",
    `identifier exceeds the maximum length of ${MAX_IDENTIFIER_LENGTH} bytes`,
  );
}
