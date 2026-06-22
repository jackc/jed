// Statement splitter — a TypeScript port of the jed CLI's splitter (cli/src/splitter.rs,
// spec/design/cli.md §4). The engine parses exactly one statement per call with no terminator
// (grammar.md §1), so the website (like the CLI) owns splitting a multi-statement editor buffer:
// `;` outside strings/comments terminates a statement; the semicolon is stripped and everything
// else — including comments, which the engine accepts — passes through verbatim. The state machine
// mirrors the engine lexer exactly: `'...'` with `''` escaping is the only quoting; `--` runs to
// end of line; `/* */` nests. Whitespace-/comment-only statements are skipped.

export type SplitStmt = { sql: string; line: number };

// SplitError is a framing error the splitter detects at end of input (an unterminated string or
// block comment) — the engine would reject it too; reporting it here gives an input line number.
export class SplitError extends Error {
  readonly line: number;
  constructor(message: string, line: number) {
    super(message);
    this.name = 'SplitError';
    this.line = line;
  }
}

type State = 'normal' | 'string' | 'lineComment' | 'blockComment';

// splitSql splits `input` into complete statements (terminator stripped, outer whitespace trimmed).
// The final statement needs no `;`. Throws SplitError on an unterminated string / block comment,
// rejecting the whole input (script semantics must not half-run a malformed buffer).
export function splitSql(input: string): SplitStmt[] {
  let state: State = 'normal';
  let depth = 0; // block-comment nesting depth
  const stmts: SplitStmt[] = [];
  let buf = '';
  // The line where the current statement's first CONTENT (non-comment, non-whitespace) char
  // appeared; null while the buffer holds only whitespace/comments (such a "statement" is skipped).
  let contentLine: number | null = null;
  let openerLine = 1; // opener line of an in-flight string / block comment (best error location)
  let line = 1;
  let i = 0;

  const finalize = () => {
    if (contentLine !== null) {
      const sql = buf.trim();
      if (sql.length > 0) stmts.push({ sql, line: contentLine });
    }
    contentLine = null;
    buf = '';
  };

  const at = (k: number): string | undefined => input[k];

  while (i < input.length) {
    const c = input[i]!;
    if (c === '\n') line += 1;

    if (state === 'normal') {
      if (c === ';') {
        finalize();
        i += 1;
      } else if (c === "'") {
        if (contentLine === null) contentLine = line;
        openerLine = line;
        state = 'string';
        buf += c;
        i += 1;
      } else if (c === '-' && at(i + 1) === '-') {
        state = 'lineComment';
        buf += '--';
        i += 2;
      } else if (c === '/' && at(i + 1) === '*') {
        openerLine = line;
        state = 'blockComment';
        depth = 1;
        buf += '/*';
        i += 2;
      } else {
        if (!/\s/.test(c)) {
          if (contentLine === null) contentLine = line;
        }
        buf += c;
        i += 1;
      }
    } else if (state === 'string') {
      if (c === "'" && at(i + 1) === "'") {
        buf += "''";
        i += 2;
      } else if (c === "'") {
        state = 'normal';
        buf += c;
        i += 1;
      } else {
        buf += c;
        i += 1;
      }
    } else if (state === 'lineComment') {
      if (c === '\n' || c === '\r') state = 'normal';
      buf += c;
      i += 1;
    } else {
      // blockComment
      if (c === '/' && at(i + 1) === '*') {
        depth += 1;
        buf += '/*';
        i += 2;
      } else if (c === '*' && at(i + 1) === '/') {
        depth -= 1;
        if (depth === 0) state = 'normal';
        buf += '*/';
        i += 2;
      } else {
        buf += c;
        i += 1;
      }
    }
  }

  if (state === 'string') throw new SplitError('unterminated string literal', openerLine);
  if (state === 'blockComment') throw new SplitError('unterminated /* comment', openerLine);
  finalize();
  return stmts;
}
