// Hand-written lexer (CLAUDE.md §5: parsers are per-language, not codegen'd). Produces
// tokens terminated by an "eof" token. Integer literals may carry a leading '-' and are
// parsed to bigint; a value outside the int64 range is a structured 42601 syntax error
// (matching Go's strconv.ParseInt(.., 64) behaviour). Errors throw EngineError.

import { engineError } from "./errors.ts";
import type { Token } from "./token.ts";

const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;

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
    } else if (c === "=") {
      tokens.push({ kind: "eq" });
      i++;
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
    } else if (c === "-" || isDigit(c)) {
      // Integer literal. A leading '-' is part of the number only when followed by a
      // digit.
      const start = i;
      if (c === "-") {
        if (!(i + 1 < n && isDigit(sql[i + 1]!))) {
          throw engineError("syntax_error", `unexpected character '${c}'`);
        }
        i++;
      }
      while (i < n && isDigit(sql[i]!)) {
        i++;
      }
      const text = sql.slice(start, i);
      const v = BigInt(text);
      if (v < I64_MIN || v > I64_MAX) {
        throw engineError("syntax_error", `integer literal out of range: ${text}`);
      }
      tokens.push({ kind: "int", int: v });
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
