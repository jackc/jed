// Hand-written lexer (CLAUDE.md §5: parsers are per-language, not codegen'd). Produces
// tokens terminated by an "eof" token. Integer literals may carry a leading '-' and are
// parsed to bigint; a value outside the i64 range is a structured 42601 syntax error
// (matching Go's strconv.ParseInt(.., 64) behaviour). Errors throw EngineError.

import { decimalFromParts, EXP_LIMIT } from "./decimal.ts";
import { engineError } from "./errors.ts";
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
export function lex(sql: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  const n = sql.length;

  while (i < n) {
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
      // `!=` is the PostgreSQL alias for `<>` (grammar.md §4); both fold to the `ne` token. A
      // lone `!` is not part of jed's surface (no factorial / boolean-not) — 42601.
      if (i + 1 < n && sql[i + 1] === "=") {
        tokens.push({ kind: "ne" });
        i += 2;
      } else {
        throw engineError("syntax_error", "unexpected character '!'");
      }
    } else if (c === "@") {
      // `@>` is the array containment operator (grammar.md §40), scanned greedily as one token;
      // a lone `@` is not part of jed's surface — 42601.
      if (i + 1 < n && sql[i + 1] === ">") {
        tokens.push({ kind: "contains" });
        i += 2;
      } else {
        throw engineError("syntax_error", "unexpected character '@'");
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
      let s = "";
      let closed = false;
      while (i < n) {
        if (sql[i] === "'") {
          if (i + 1 < n && sql[i + 1] === "'") {
            s += "'";
            i += 2;
            continue;
          }
          i++; // consume the closing quote
          closed = true;
          break;
        }
        s += sql[i]!;
        i++;
      }
      if (!closed) {
        throw engineError("syntax_error", "unterminated string literal");
      }
      tokens.push({ kind: "str", str: s });
    } else if (c === '"') {
      // Double-quoted identifier (collation names, spec/design/collation.md §1). `""` is an embedded
      // double quote; the content is kept VERBATIM (case-sensitive). The parser rejects an empty
      // name; an empty `""` lexes fine here.
      i++; // consume the opening quote
      let s = "";
      let closed = false;
      while (i < n) {
        if (sql[i] === '"') {
          if (i + 1 < n && sql[i + 1] === '"') {
            s += '"';
            i += 2;
            continue;
          }
          i++; // consume the closing quote
          closed = true;
          break;
        }
        s += sql[i]!;
        i++;
      }
      if (!closed) {
        throw engineError("syntax_error", "unterminated quoted identifier");
      }
      tokens.push({ kind: "quotedIdent", str: s });
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
        throw engineError(
          "name_too_long",
          `identifier exceeds the maximum length of ${MAX_IDENTIFIER_LENGTH} bytes`,
        );
      }
      tokens.push({ kind: "word", word: sql.slice(start, i) });
    } else {
      throw engineError("syntax_error", `unexpected character '${c}'`);
    }
  }

  tokens.push({ kind: "eof" });
  return tokens;
}
