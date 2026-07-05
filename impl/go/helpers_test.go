package jed

// White-box test helpers that materialize the production queryValues seam. The public API has one
// result type — *Rows (a statement is a *Rows with no output columns, carrying the command tag). These
// helpers drain that cursor into an outcome so a test can assert on the full result set + tag at once,
// exactly the shape the removed Execute→Outcome API returned, but built over the seam callers actually
// use. Tests call queryOutcome/prepOutcome instead of a test-only Execute (CLAUDE.md §10: prefer the
// real surface; a helper exists only for what a bare cursor makes verbose — draining every row).

// valueQuerier is the raw production seam (sql, []Value) -> *Rows. *Session, *Database, *engine, and
// *Transaction all expose it.
type valueQuerier interface {
	queryValues(sql string, params []Value) (*Rows, error)
}

// queryOutcome runs sql through the real queryValues seam and materializes the cursor into an outcome.
func queryOutcome(q valueQuerier, sql string, params []Value) (outcome, error) {
	rows, err := q.queryValues(sql, params)
	if err != nil {
		return outcome{}, err
	}
	return drainOutcome(rows)
}

// prepOutcome is queryOutcome for a prepared statement, run on s — a statement is a standalone
// value; the handle passed at each execute supplies the session (spec/design/api.md §2.4).
func prepOutcome(s *Session, p *PreparedStatement, params []Value) (outcome, error) {
	rows, err := s.queryStmt(p.ast, params, &p.sc)
	if err != nil {
		return outcome{}, err
	}
	return drainOutcome(rows)
}

// drainOutcome pulls a cursor to exhaustion and packages the result set + command tag as an outcome.
// Cost and RowsAffected are read after the drain (a streaming cursor accrues cost as it is pulled).
func drainOutcome(rows *Rows) (outcome, error) {
	defer rows.Close()
	out := outcome{
		ColumnNames: rows.ColumnNames(),
		ColumnTypes: rows.ColumnTypes(),
	}
	for rows.Next() {
		out.Rows = append(out.Rows, append([]Value(nil), rows.Row()...))
	}
	if err := rows.Err(); err != nil {
		return outcome{}, err
	}
	out.Cost = rows.Cost()
	out.RowsAffected, out.HasRowsAffected = rows.RowsAffected()
	// A result carrying output columns is a query; otherwise a bare statement (the total-queryValues
	// contract — a no-column cursor IS the statement outcome).
	if len(out.ColumnNames) > 0 {
		out.Kind = outcomeQuery
	} else {
		out.Kind = outcomeStatement
	}
	return out, nil
}
