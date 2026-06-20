// Library-level multi-statement splitter (spec/design/session.md §4.1). A pure, streaming statement
// scanner that depends on NEITHER Session nor Database — a top-level core export, conceptually part
// of the lexer surface (CLAUDE.md §5: parsers are per-language, not codegen'd), callable before any
// database is opened. It yields one statement's source text at a time, lazily (a generator),
// buffering nothing across statements (an O(n) scan, no parse tree).
//
// It scans so a `;` inside a string literal, a dollar-quoted string, or a line/block comment is
// never a statement boundary. It does NOT validate tokens (a lone `!` is just a character to the
// splitter — the error surfaces when the host feeds the span to parse), so it never throws. Empty
// spans — a leading/standalone `;`, or whitespace/comment-only text between separators — are
// skipped, so every yielded span has significant content. Offsets are UTF-16 code-unit indices
// (the natural JS string index); they coincide with the Rust/Go byte offsets for ASCII and are for
// the host's own error reporting (the splitter is per-core unit tested, not cross-core byte-compared).

// StatementSpan is one statement carved out of a multi-statement string. `text` is the statement's
// source (feed it to execute/query/prepare) and `offset` is the index of its first significant
// character in the original input.
export type StatementSpan = { text: string; offset: number };

// ScriptSummary is the O(1) summary of an executeScript run (spec/design/session.md §4.2). It carries
// only counts — never the result rows, which executeScript discards — so memory is bounded by
// construction regardless of how many rows the script's statements touch. (cost is a bigint for i64
// parity with Outcome; rowsAffectedTotal sums the DML command-tag counts — a SELECT or DDL adds 0.)
export type ScriptSummary = {
  statementsRun: number;
  rowsAffectedTotal: number;
  cost: bigint;
};

// splitStatements lazily yields the top-level statements in sql (spec/design/session.md §4.1),
// splitting on top-level `;` while respecting string literals, dollar-quoted strings, and
// line/block comments.
export function* splitStatements(sql: string): Generator<StatementSpan> {
  const n = sql.length;
  let i = 0;
  // start = the first significant char of the current statement (-1 until one is seen);
  // lastEnd = one past the last significant char (so trailing whitespace/comments trim off).
  let start = -1;
  let lastEnd = 0;

  while (i < n) {
    const ch = sql[i];
    if (ch === " " || ch === "\t" || ch === "\r" || ch === "\n") {
      i++;
    } else if (ch === ";") {
      i++;
      if (start >= 0) {
        yield { text: sql.slice(start, lastEnd), offset: start };
        start = -1;
      }
      // An empty span (leading or standalone `;`) — keep scanning.
    } else if (ch === "-" && sql[i + 1] === "-") {
      // `--` line comment to end of line (non-significant — never sets start).
      i += 2;
      while (i < n && sql[i] !== "\n" && sql[i] !== "\r") i++;
    } else if (ch === "/" && sql[i + 1] === "*") {
      // `/* … */` block comment; blocks NEST (PG / the lexer). An unterminated comment runs to EOF
      // and stays non-significant (a comment-only tail is an empty span).
      i += 2;
      let depth = 1;
      while (depth > 0 && i < n) {
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
    } else if (ch === "'") {
      // Single-quoted string literal; `''` is an embedded quote. An unterminated literal runs to EOF.
      if (start < 0) start = i;
      i++;
      while (i < n) {
        if (sql[i] === "'") {
          if (sql[i + 1] === "'") {
            i += 2;
          } else {
            i++;
            break;
          }
        } else {
          i++;
        }
      }
      lastEnd = i;
    } else if (ch === "$") {
      // A `$tag$ … $tag$` dollar-quoted string (PG): `$` + an optional identifier tag + `$`, closed
      // by the same delimiter. `$1` (a digit follows) is a bind parameter, not a dollar-quote — one
      // ordinary significant character.
      if (start < 0) start = i;
      const tagLen = dollarTagLen(sql, i);
      if (tagLen > 0) {
        const open = sql.slice(i, i + tagLen);
        let j = i + tagLen;
        for (;;) {
          if (j >= n) {
            j = n; // unterminated — consume to EOF
            break;
          }
          if (sql[j] === "$" && sql.slice(j, j + tagLen) === open) {
            j += tagLen; // matched closing delimiter
            break;
          }
          j++;
        }
        i = j;
      } else {
        i++;
      }
      lastEnd = i;
    } else {
      if (start < 0) start = i;
      i++;
      lastEnd = i;
    }
  }

  // End of input: emit the trailing statement if it had significant content.
  if (start >= 0) yield { text: sql.slice(start, lastEnd), offset: start };
}

// dollarTagLen returns the total length of a dollar-quote opening delimiter `$tag$` at sql[i] (where
// sql[i] === "$"), including both `$`; 0 if sql[i] does not open one. A tag is empty (`$$`) or
// [A-Za-z_][A-Za-z0-9_]* (PG); a `$` followed by a digit (`$1`) or with no terminating `$` is not a
// dollar-quote opener.
function dollarTagLen(sql: string, i: number): number {
  let j = i + 1;
  if (sql[j] === "$") return 2; // empty tag: `$$`
  const first = sql[j];
  if (first === undefined || !(isTagAlpha(first) || first === "_")) return 0;
  j++;
  while (j < sql.length && (isTagAlpha(sql[j]) || isTagDigit(sql[j]) || sql[j] === "_")) j++;
  if (sql[j] === "$") return j + 1 - i;
  return 0; // no terminating `$` for the tag
}

function isTagAlpha(c: string): boolean {
  return (c >= "a" && c <= "z") || (c >= "A" && c <= "Z");
}
function isTagDigit(c: string): boolean {
  return c >= "0" && c <= "9";
}
