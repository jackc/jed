package jed

// Host API surface for the Go core (spec/design/api.md §1): prepare a statement, execute or
// query it (with optional $N bind parameters), and iterate result rows. Thin wrappers over the
// parser + executor — the conformance contract still binds (the executor is unchanged).

// PreparedStatement is a parsed, reusable statement (spec/design/api.md §2.4). It holds the
// parsed AST and a back-pointer to the database it was prepared against (Go is GC'd, so binding
// the database at prepare is safe — unlike Rust's borrow model, api.md §6).
type PreparedStatement struct {
	db  *Database
	ast Statement
}

// Prepare parses sql once into a reusable prepared statement (spec/design/api.md §2.4). Parse
// errors (42601, …) surface here.
func (db *Database) Prepare(sql string) (*PreparedStatement, error) {
	stmt, err := ParseSQL(sql)
	if err != nil {
		return nil, err
	}
	return &PreparedStatement{db: db, ast: stmt}, nil
}

// Execute runs this statement, binding params to its $N placeholders (nil when it has none),
// returning the materialized outcome.
func (s *PreparedStatement) Execute(params []Value) (Outcome, error) {
	return s.db.ExecuteStmtParams(s.ast, params)
}

// Query runs this query statement, returning a row cursor. A non-query statement is a 42601
// (use Execute).
func (s *PreparedStatement) Query(params []Value) (*Rows, error) {
	out, err := s.Execute(params)
	if err != nil {
		return nil, err
	}
	return rowsFromOutcome(out)
}

// ExecuteSQL is a one-shot: parse + execute sql, binding params, returning the outcome. (The
// package function Execute(db, sql) is the zero-parameter convenience kept for back-compat.)
func (db *Database) ExecuteSQL(sql string, params []Value) (Outcome, error) {
	return ExecuteParams(db, sql, params)
}

// QuerySQL is a one-shot: parse + run a query sql, binding params, returning a row cursor.
func (db *Database) QuerySQL(sql string, params []Value) (*Rows, error) {
	out, err := ExecuteParams(db, sql, params)
	if err != nil {
		return nil, err
	}
	return rowsFromOutcome(out)
}

// Rows is a cursor over a query's rows (spec/design/api.md §4). It walks the materialized result
// one row at a time (true streaming is deferred per CLAUDE.md §9 — the cursor contract is the
// seam that lets the source become lazy later without a caller change), and exposes the column
// names and the accrued execution cost.
type Rows struct {
	columnNames []string
	rows        [][]Value
	idx         int
	cost        int64
}

func rowsFromOutcome(out Outcome) (*Rows, error) {
	if out.Kind != OutcomeQuery {
		return nil, NewError(SyntaxError, "Query called on a statement that produces no rows; use Execute")
	}
	return &Rows{columnNames: out.ColumnNames, rows: out.Rows, cost: out.Cost}, nil
}

// Next advances to the next row, returning false when the result is exhausted.
func (r *Rows) Next() bool {
	if r.idx >= len(r.rows) {
		return false
	}
	r.idx++
	return true
}

// Row returns the current row (valid after a Next that returned true).
func (r *Rows) Row() []Value { return r.rows[r.idx-1] }

// ColumnNames are the output column names of the query result.
func (r *Rows) ColumnNames() []string { return r.columnNames }

// Cost is the deterministic execution cost accrued by the query (CLAUDE.md §13).
func (r *Rows) Cost() int64 { return r.cost }
