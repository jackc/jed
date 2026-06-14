// Hand-written lexer (CLAUDE.md §5: parsers are per-language, not codegen'd). Produces
// tokens terminated by an "eof" token. Integer literals may carry a leading '-' and are
// parsed to bigint; a value outside the int64 range is a structured 42601 syntax error
// (matching Go's strconv.ParseInt(.., 64) behaviour). Errors throw EngineError.

import { engineError } from "./errors.ts";
import type { Token } from "./token.ts";

// The maximum integer-literal MAGNITUDE the lexer accepts: 2^63, so that the unary
// minus of it folds to int64's minimum. A larger magnitude cannot be represented.
const MAX_MAGNITUDE = 9223372036854775808n;

function isDigit(c: string): boolean {
  return c >= "0" && c <= "9";
}

function isAlpha(c: string): boolean {
  return c === "_" || (c >= "a" && c <= "z") || (c >= "A" && c <= "Z");
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
    } else if (c === "*") {
      tokens.push({ kind: "star" });
      i++;
    } else if (c === "+") {
      tokens.push({ kind: "plus" });
      i++;
    } else if (c === "-") {
      // `--` starts a line comment running to the end of the line; comments are
      // whitespace (grammar.md §33). Two hyphens ALWAYS start a comment outside a
      // string, even abutting a token (`1--2` is `1` — PG behavior).
      if (i + 1 < n && sql[i + 1] === "-") {
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
    } else if (c === ":") {
      // `::` is the PostgreSQL typecast operator (grammar.md §37), scanned greedily as one
      // token. A lone `:` is not part of jed's surface — a 42601 syntax error.
      if (i + 1 < n && sql[i + 1] === ":") {
        tokens.push({ kind: "doubleColon" });
        i += 2;
      } else {
        throw engineError("syntax_error", "unexpected character ':'");
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
      } else {
        tokens.push({ kind: "lt" });
        i++;
      }
    } else if (c === ">") {
      if (i + 1 < n && sql[i + 1] === "=") {
        tokens.push({ kind: "ge" });
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
    } else if (isDigit(c)) {
      // A numeric literal. Scan the integer digits; if a "." follows it is a DECIMAL literal
      // (scan the fractional digits), else an INTEGER literal.
      const start = i;
      while (i < n && isDigit(sql[i]!)) {
        i++;
      }
      if (i < n && sql[i] === ".") {
        // Decimal: `123.`, `123.45`. The fractional part may be empty (`1.`).
        const intPart = sql.slice(start, i);
        i++; // consume "."
        const fracStart = i;
        while (i < n && isDigit(sql[i]!)) {
          i++;
        }
        const frac = sql.slice(fracStart, i);
        tokens.push({ kind: "decimal", decDigits: intPart + frac, decScale: frac.length });
      } else {
        // Integer literal: an unsigned magnitude (the sign is the "minus" operator). The
        // magnitude must be <= 2^63 so that -(2^63) = int64's minimum is reachable; anything
        // larger cannot be represented (42601).
        const text = sql.slice(start, i);
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
        tokens.push({ kind: "decimal", decDigits: frac, decScale: frac.length });
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
      tokens.push({ kind: "word", word: sql.slice(start, i) });
    } else {
      throw engineError("syntax_error", `unexpected character '${c}'`);
    }
  }

  tokens.push({ kind: "eof" });
  return tokens;
}
