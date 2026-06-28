package jed

import (
	"bytes"
	"iter"
)

// split.go — the library-level multi-statement splitter (spec/design/session.md §4.1) plus the
// session-level ExecuteScript convenience (§4.2). The splitter depends on NEITHER Session nor
// Engine — a top-level core export, conceptually part of the lexer surface (CLAUDE.md §5).

// StatementSpan is one statement carved out of a multi-statement string (spec/design/session.md
// §4.1). Text is the statement's source — feed it to ExecuteSQL/QuerySQL/Prepare — and Offset is
// the byte offset of its first significant byte in the original input (for error reporting).
type StatementSpan struct {
	Text   string
	Offset int
}

// SplitStatements returns a lazy iterator over the statements in sql (spec/design/session.md §4.1),
// splitting on top-level `;` while respecting string literals, dollar-quoted strings, and
// line/block comments — a `;` inside any of those is never a boundary. It is pure and total (no
// Session/Engine, never an error: a lone `!` is just a byte to the splitter — the parse error is
// the host's when it runs the span). Empty spans (a leading/standalone `;`, or whitespace/comment-
// only text between separators) are skipped, so every yielded span has significant content; the
// text has leading/trailing whitespace and comments trimmed (interior comments kept). It buffers
// nothing across statements — an O(n) scan, no parse tree.
func SplitStatements(sql string) iter.Seq[StatementSpan] {
	return func(yield func(StatementSpan) bool) {
		b := []byte(sql)
		n := len(b)
		i := 0
		// start = the first significant byte of the current statement (-1 until one is seen);
		// lastEnd = one past the last significant byte (so trailing whitespace/comments trim off).
		start := -1
		lastEnd := 0
		for i < n {
			c := b[i]
			switch {
			case c == ' ' || c == '\t' || c == '\r' || c == '\n':
				i++
			case c == ';':
				i++
				if start >= 0 {
					if !yield(StatementSpan{Text: sql[start:lastEnd], Offset: start}) {
						return
					}
					start = -1
				}
				// An empty span (leading or standalone `;`) — keep scanning.
			case c == '-' && i+1 < n && b[i+1] == '-':
				// `--` line comment to end of line (non-significant — never sets start).
				i += 2
				for i < n && b[i] != '\n' && b[i] != '\r' {
					i++
				}
			case c == '/' && i+1 < n && b[i+1] == '*':
				// `/* … */` block comment; blocks NEST (PG / the lexer). An unterminated comment
				// runs to EOF and stays non-significant (a comment-only tail is an empty span).
				i += 2
				depth := 1
				for depth > 0 && i < n {
					if b[i] == '/' && i+1 < n && b[i+1] == '*' {
						depth++
						i += 2
					} else if b[i] == '*' && i+1 < n && b[i+1] == '/' {
						depth--
						i += 2
					} else {
						i++
					}
				}
			case c == '\'':
				// Single-quoted string literal; `''` is an embedded quote. An unterminated literal
				// runs to EOF (the parse error is the host's).
				if start < 0 {
					start = i
				}
				i++
				for i < n {
					if b[i] == '\'' {
						if i+1 < n && b[i+1] == '\'' {
							i += 2
						} else {
							i++
							break
						}
					} else {
						i++
					}
				}
				lastEnd = i
			case c == '$':
				// A `$tag$ … $tag$` dollar-quoted string (PG): `$` + an optional identifier tag +
				// `$`, closed by the same delimiter. `$1` (a digit follows) is a bind parameter, not
				// a dollar-quote — one ordinary significant byte.
				if start < 0 {
					start = i
				}
				if tagLen := dollarTagLen(b, i); tagLen > 0 {
					open := b[i : i+tagLen]
					j := i + tagLen
					for {
						if j >= n {
							j = n // unterminated — consume to EOF
							break
						}
						if b[j] == '$' && j+tagLen <= n && bytes.Equal(b[j:j+tagLen], open) {
							j += tagLen // matched closing delimiter
							break
						}
						j++
					}
					i = j
				} else {
					i++
				}
				lastEnd = i
			default:
				if start < 0 {
					start = i
				}
				i++
				lastEnd = i
			}
		}
		// End of input: emit the trailing statement if it had significant content.
		if start >= 0 {
			yield(StatementSpan{Text: sql[start:lastEnd], Offset: start})
		}
	}
}

// dollarTagLen returns the total length of a dollar-quote opening delimiter `$tag$` at b[i] (where
// b[i] == '$'), including both `$`; 0 if b[i] does not open one. A tag is empty (`$$`) or
// [A-Za-z_][A-Za-z0-9_]* (PG); a `$` followed by a digit (`$1`) or with no terminating `$` is not a
// dollar-quote opener.
func dollarTagLen(b []byte, i int) int {
	j := i + 1
	if j < len(b) && b[j] == '$' {
		return 2 // empty tag: `$$`
	}
	if j >= len(b) || !(isTagAlpha(b[j]) || b[j] == '_') {
		return 0
	}
	j++
	for j < len(b) && (isTagAlpha(b[j]) || isTagDigit(b[j]) || b[j] == '_') {
		j++
	}
	if j < len(b) && b[j] == '$' {
		return j + 1 - i
	}
	return 0
}

func isTagAlpha(c byte) bool { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') }
func isTagDigit(c byte) bool { return c >= '0' && c <= '9' }

// ScriptSummary is the O(1) summary of an ExecuteScript run (spec/design/session.md §4.2). It
// carries only counts — never the result rows, which ExecuteScript discards — so memory is bounded
// by construction regardless of how many rows the script's statements touch.
type ScriptSummary struct {
	// StatementsRun is how many statements ran (each non-empty span the splitter yielded).
	StatementsRun int64
	// RowsAffectedTotal is the sum of the DML command-tag counts (INSERT/UPDATE/DELETE rows
	// affected). A SELECT or a DDL/transaction-control statement contributes nothing.
	RowsAffectedTotal int64
	// Cost is the total accrued execution cost across every statement (the deterministic cost
	// meter, CLAUDE.md §13) — the figure a future lifetime_max_cost budget bounds.
	Cost int64
}

// ExecuteScript runs a multi-statement sql SCRIPT on the default session (spec/design/session.md
// §4.2): split it, run each statement in order, DISCARD the result rows (keeping only counts), and
// return the O(1) ScriptSummary. The dominant migration/import path — "run this script; I only
// care that it succeeded."
//
//   - Idle at entry  ⇒ the whole run is one implicit transaction, all-or-nothing: a statement error
//     rolls the wrapper back (nothing is committed) and returns that error.
//   - Open at entry  ⇒ the run joins that transaction (no wrapper, no auto-commit); a mid-run error
//     leaves the block Failed for the caller to roll back.
//   - In-script transaction control (BEGIN/COMMIT/ROLLBACK) is 0A000 — the implicit wrapper owns the
//     boundary (partitioning is deferred, session.md §11). A host that needs self-managed
//     transactions writes its own SplitStatements loop instead.
func (db *engine) ExecuteScript(sql string) (ScriptSummary, error) {
	ownsWrapper := !db.InTransaction()
	if ownsWrapper {
		// The implicit wrapper honors the handle's read-only mode (modeSet=false ⇒ READ ONLY on a
		// read-only handle — a write inside is 25006, exactly like autocommit).
		if _, err := db.beginTx(false, false); err != nil {
			return ScriptSummary{}, err
		}
	}
	summary, err := db.runScriptBody(sql)
	if err != nil {
		if ownsWrapper {
			_ = db.Rollback() // discard everything; surface the original error
		}
		return ScriptSummary{}, err
	}
	if ownsWrapper {
		if cerr := db.Commit(); cerr != nil { // publish the all-or-nothing run
			return ScriptSummary{}, cerr
		}
	}
	return summary, nil
}

// runScriptBody splits sql and runs each statement on the current transaction, accumulating the
// ScriptSummary. Separated so ExecuteScript's wrapper commit/rollback runs once on either path.
func (db *engine) runScriptBody(sql string) (ScriptSummary, error) {
	var summary ScriptSummary
	for span := range SplitStatements(sql) {
		ast, err := db.parse(span.Text)
		if err != nil {
			return ScriptSummary{}, err
		}
		// Transaction control inside a script is the v1 narrowing (session.md §4.2): the implicit
		// wrapper owns the boundary, so BEGIN/COMMIT/ROLLBACK is 0A000 (partitioning deferred).
		if ast.Begin != nil || ast.Commit != nil || ast.Rollback != nil {
			return ScriptSummary{}, newError(FeatureNotSupported,
				"transaction control (BEGIN/COMMIT/ROLLBACK) is not supported inside execute_script; "+
					"use SplitStatements to run a self-managed multi-statement transaction")
		}
		out, err := db.ExecuteStmtParams(ast, nil)
		if err != nil {
			return ScriptSummary{}, err
		}
		summary.StatementsRun++
		if out.HasRowsAffected {
			summary.RowsAffectedTotal += out.RowsAffected
		}
		summary.Cost += out.Cost
	}
	return summary, nil
}
