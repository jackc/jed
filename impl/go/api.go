package jed

// Host API surface for the Go core (spec/design/api.md §1): prepare a statement, execute or
// query it (with optional $N bind parameters), and iterate result rows. Thin wrappers over the
// parser + executor — the conformance contract still binds (the executor is unchanged).

// Transaction is an open explicit transaction (spec/design/api.md §2.2, transactions.md §4.4).
// Statements run through Execute/Query; Commit/Rollback end it. Go has no destructor, so a raw
// Begin caller must end it explicitly — View/Update do that automatically (and are preferred).
type Transaction struct {
	db   *engine
	done bool
}

// Execute runs a (possibly mutating) statement within this transaction, binding params. A write
// in a READ ONLY transaction is 25006; a statement error aborts the block (every later statement
// but commit/rollback is then 25P02).
func (tx *Transaction) Execute(sql string, params []Value) (Outcome, error) {
	return tx.db.ExecuteSQL(sql, params)
}

// Query runs a query within this transaction, returning a row cursor.
func (tx *Transaction) Query(sql string, params []Value) (*Rows, error) {
	return tx.db.QuerySQL(sql, params)
}

// Commit publishes the transaction durably (per synchronous). Idempotent after the transaction
// has ended.
func (tx *Transaction) Commit() error {
	if tx.done {
		return nil
	}
	tx.done = true
	return tx.db.Commit()
}

// Rollback discards the transaction's working set. Idempotent after the transaction has ended, so
// a `defer tx.Rollback()` after a Commit is a safe no-op (the View/Update wrappers rely on this).
func (tx *Transaction) Rollback() error {
	if tx.done {
		return nil
	}
	tx.done = true
	return tx.db.Rollback()
}

// Begin opens an explicit transaction (spec/design/api.md §2.2). writable false is READ ONLY (a
// write inside → 25006); true is READ WRITE. A nested Begin (a transaction is already open) is
// 25001. Prefer View/Update, which cannot forget to end the transaction.
func (db *engine) Begin(writable bool) (*Transaction, error) {
	if _, err := db.beginTx(writable, true); err != nil {
		return nil, err
	}
	return &Transaction{db: db}, nil
}

// View runs fn in a READ ONLY transaction (bbolt-style): open it, run fn(tx), then auto-commit on
// success / auto-rollback on error or panic. A write inside is 25006.
func (db *engine) View(fn func(tx *Transaction) error) error {
	return db.withTx(false, fn)
}

// Update runs fn in a READ WRITE transaction (bbolt-style): open it, run fn(tx), then auto-commit
// on success / auto-rollback on error or panic — the safe default over a raw Begin.
func (db *engine) Update(fn func(tx *Transaction) error) error {
	return db.withTx(true, fn)
}

func (db *engine) withTx(writable bool, fn func(tx *Transaction) error) error {
	tx, err := db.Begin(writable)
	if err != nil {
		return err
	}
	// Rolls back on an early return or panic; a no-op once Commit has ended the transaction.
	defer func() { _ = tx.Rollback() }()
	if err := fn(tx); err != nil {
		return err
	}
	return tx.Commit()
}

// PreparedStatement is a parsed, reusable statement (spec/design/api.md §2.4). It holds the
// parsed AST and a back-pointer to the handle it was prepared against (Go is GC'd, so binding
// the handle at prepare is safe — unlike Rust's borrow model, api.md §6). When prepared on a
// Session/Database (sess set) Execute routes through the session's dispatch — the lazy writer
// gate for writes, the pinned snapshot for reads — so it observes the converged §2.4 semantics.
type PreparedStatement struct {
	db   *engine
	sess *Session
	ast  statement
}

// Prepare parses sql once into a reusable prepared statement (spec/design/api.md §2.4). Parse
// errors (42601, …) surface here.
func (db *engine) Prepare(sql string) (*PreparedStatement, error) {
	stmt, err := db.parse(sql)
	if err != nil {
		return nil, err
	}
	return &PreparedStatement{db: db, ast: stmt}, nil
}

// Execute runs this statement, binding params to its $N placeholders (nil when it has none),
// returning the materialized outcome.
func (s *PreparedStatement) Execute(params []Value) (Outcome, error) {
	if s.sess != nil {
		return s.sess.dispatch(s.ast, params)
	}
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
func (db *engine) ExecuteSQL(sql string, params []Value) (Outcome, error) {
	return executeParams(db, sql, params)
}

// QuerySQL is a one-shot: parse + run a query sql, binding params, returning a row cursor.
func (db *engine) QuerySQL(sql string, params []Value) (*Rows, error) {
	out, err := executeParams(db, sql, params)
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
		return nil, newError(SyntaxError, "Query called on a statement that produces no rows; use Execute")
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
