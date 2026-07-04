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

// QueryValues runs a statement within this transaction, returning a row cursor (the raw []Value
// path; the ergonomic Query/Exec(ctx, sql, args...) in ergonomic.go owns the Query name — api.md §11).
// Total: a non-query statement (a write in a READ ONLY transaction is 25006; a statement error aborts
// the block, every later statement but commit/rollback then 25P02) returns a no-column cursor carrying
// the command tag.
func (tx *Transaction) QueryValues(sql string, params []Value) (*Rows, error) {
	return tx.db.QueryValues(sql, params)
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
	// sc memoizes the resolved scan plan across QueryValues calls so a repeated execute skips
	// planning (the plan cache, spec/design/api.md §2.4). Populated lazily on the first cacheable
	// query execute and invalidated automatically when the catalog changes (scanCache.catGen). Zero
	// value is empty. Query-only — Execute (writes / materialized shapes) never touches it.
	sc scanCache
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

// QueryValues runs this prepared statement, returning a row cursor. The prepared AST routes through the
// same lazy streaming / buffered / deferred lanes as the ad-hoc QueryValues (spec/design/streaming.md §3/§4/§7)
// — so a prepared query streams exactly like a one-shot one. When prepared on a Session (sess set) it
// routes through the session (the pinned snapshot + watermark, the converged §2.4 semantics); a bare
// engine prepared statement routes through the engine. Total: a non-query statement returns a no-column
// cursor carrying the command tag. This is the raw []Value path; the ergonomic Query/Exec(ctx, args...)
// in ergonomic.go owns the Query name (api.md §11).
func (s *PreparedStatement) QueryValues(params []Value) (*Rows, error) {
	if s.sess != nil {
		return s.sess.queryStmt(s.ast, params, &s.sc)
	}
	return s.db.queryStmt(s.ast, params, &s.sc)
}

// QueryValues is a one-shot: parse + run a statement, binding params, returning a row cursor. A
// single-table no-blocking-operator read is served by a lazy STREAMING cursor (spec/design/streaming.md
// §4, S3); a blocking read (ORDER BY/DISTINCT/aggregate/window/join) by a lazy BUFFERED cursor (S4)
// that buffers the input but yields the output one row at a time. Both pull over a pinned snapshot with
// bounded peak output memory and a caller early-exit; a top-level set operation / pure-query WITH is
// served by a lazy DEFERRED cursor (streaming.md §7) that defers the run to the first pull and yields
// the result one row at a time. Total: a non-query statement returns a no-column cursor carrying the
// command tag. (This is the bare single-handle engine; the watermark pin lives on the shared-core
// Session.QueryValues path.)
func (db *engine) QueryValues(sql string, params []Value) (*Rows, error) {
	stmt, err := db.parse(sql)
	if err != nil {
		return nil, err
	}
	return db.queryStmt(stmt, params, nil) // one-shot: no cross-call plan cache (still plans once)
}

// queryStmt routes an already-parsed query AST through the lazy scan (streaming/buffered) then
// deferred lanes, planning a scan-shaped SELECT exactly once (tryScanQuery), and falling back to the
// materialized ExecuteStmtParams for a shape no lazy lane covers (a write, a nextval/setval SELECT, a
// data-modifying WITH). Shared by QueryValues (parse-then-route, sc nil) and a prepared query
// (PreparedStatement.QueryValues passes its scanCache), so a prepared query streams identically to an
// ad-hoc one but reuses its cached plan across executes. (This is the bare single-handle engine; the
// watermark pin lives on the shared-core Session.QueryValues path.)
func (db *engine) queryStmt(stmt statement, params []Value, sc *scanCache) (*Rows, error) {
	// A read served by a lazy lane skips the materialized dispatch, so enforce the read-path admission
	// gates (25P02 / 54P02 / 42501) up front — reads only (transaction control must work in a failed
	// block; a write is gated inside dispatch on the fall-through). Keeps the bare-engine QueryValues a
	// safe total seam like the shared-core Session.QueryValues (executor.go gateReadLanes, CLAUDE.md §13).
	if stmt.Begin == nil && stmt.Commit == nil && stmt.Rollback == nil && !stmtIsWrite(stmt) {
		if err := db.gateReadLanes(stmt); err != nil {
			return nil, db.poisonOnLaneErr(err)
		}
	}
	if rows, ok, err := db.tryScanQuery(stmt, params, sc); err != nil {
		return nil, db.poisonOnLaneErr(err)
	} else if ok {
		return db.attachBlockPoison(rows), nil
	}
	if rows, ok, err := db.tryDeferredQuery(stmt, params); err != nil {
		return nil, db.poisonOnLaneErr(err)
	} else if ok {
		return db.attachBlockPoison(rows), nil
	}
	// The fall-through handles transaction control (BEGIN/COMMIT/ROLLBACK — a nested BEGIN's 25001 must
	// NOT poison) and self-poisons on a regular statement error (ExecuteStmtParams), so its nuanced
	// poisoning is left intact — only the lazy-lane reads above, which bypass it, are poisoned here.
	out, err := db.ExecuteStmtParams(stmt, params)
	if err != nil {
		return nil, err
	}
	return rowsFromOutcome(out), nil
}

// Rows is a cursor over a statement's rows (spec/design/api.md §4). It walks the pull source (cursor.go)
// one row at a time, exposing the column names, command tag, and accrued execution cost. The source is
// buffered for the materialized drive (a write / a shape no lazy lane covers) and a lazy streaming pull
// pipeline (S3, streaming.md §4) for a single-table no-blocking-op read — the seam is the same either
// way, so callers are unchanged. It is the one result type of the total QueryValues seam: a non-query
// statement is a Rows with no output columns, carrying its command tag (rowsAffected).
type Rows struct {
	columnNames []string
	columnTypes []string
	// rowsAffected / hasAffected carry the command tag (spec/design/api.md §4) for a statement run
	// through the now-total query seam (rowsFromOutcome): how many rows a DML statement touched, and
	// whether it carries a count at all (a SELECT / DDL / transaction control has none). This is how
	// the exec-side path (ergoExec) reads the tag off a drained Rows — "Exec is throw away the rows".
	rowsAffected int64
	hasAffected  bool
	// cursor is the pull source (cursor.go): a bufCursor over a materialized result, or a
	// streamingCursor (S3, executor.go) running scan → resolve → WHERE → project lazily over a pinned
	// snapshot (streaming.md §4).
	cursor cursor
	// onClose is the reader-liveness pin's deregister (the watermark, streaming.md §5) — set by
	// Session.QueryValues for a streaming cursor, called by Close (Go has no destructor; the ergonomic
	// iterators close on loop exit). nil for a buffered cursor or a bare single-handle stream.
	onClose func()
	// current is the row the last successful Next produced; valid reports whether there is one.
	current []Value
	valid   bool
	// ctx is captured at Query time (ergonomic.go); Next polls it so a canceled context aborts
	// iteration. Today the result is already materialized, so this is the forward-compatible hook
	// for the streaming cursor (CLAUDE.md §9) — the deeper integration threads ctx through the
	// cost meter's Guard() so the materialize itself aborts (spec/design/api.md §11).
	ctx ctxIface
	// err holds a terminal error reached during iteration (a canceled ctx, a future mid-stream
	// fault). Surfaced by Err() after the loop — the bufio.Scanner / database/sql idiom.
	err error
	// onErr fires once when a terminal iteration error is first set (a drain-time streaming/deferred
	// fault). Set by Session/engine.queryStmt (attachBlockPoison) for a read inside an open block, to
	// abort the block — the open-time lane errors are poisoned at the queryStmt returns, this covers the
	// errors that surface only during the caller's Next(). nil for an autocommit read or a buffered
	// cursor (whose error surfaces at open, not drain).
	onErr func(error)
}

// rowsFromOutcome wraps a materialized outcome (the executor's internal result) as a *Rows. It is
// TOTAL: a non-query statement is observably a Rows with no output columns — an empty buffered cursor
// seeded with the accrued cost, carrying the statement's command tag (rows-affected). This is the
// single exec/query seam: the exec-side path (Exec) drains-and-discards such a Rows and returns the
// tag, so "Query on a statement that produces no rows" is valid, not a 42601 (the effect-then-error
// bug this removes — a write reached here after dispatch already committed it; spec/design/api.md §11).
func rowsFromOutcome(out outcome) *Rows {
	return &Rows{
		columnNames:  out.ColumnNames,
		columnTypes:  out.ColumnTypes,
		cursor:       bufferedCursor(out.Rows, out.Cost),
		rowsAffected: out.RowsAffected,
		hasAffected:  out.HasRowsAffected,
	}
}

// attachPin records the reader-liveness pin's deregister for a streaming cursor (the watermark,
// spec/design/streaming.md §5); Close calls it.
func (r *Rows) attachPin(deregister func()) { r.onClose = deregister }

// attachErrHook records a callback fired once when iteration first hits a terminal error (queryStmt's
// attachBlockPoison uses it to abort an open block on a drain-time read fault).
func (r *Rows) attachErrHook(hook func(error)) { r.onErr = hook }

// setErr records a terminal iteration error and fires the onErr hook exactly once (subsequent Next
// short-circuits on r.err, so the hook cannot double-fire).
func (r *Rows) setErr(err error) {
	r.err = err
	if r.onErr != nil {
		r.onErr(err)
		r.onErr = nil
	}
}

// Next advances to the next row, returning false when the result is exhausted OR the captured
// context has been canceled (Err then reports the cancellation).
func (r *Rows) Next() bool {
	r.valid = false
	if r.err != nil {
		return false
	}
	if err := ctxErr(r.ctx); err != nil {
		r.setErr(err)
		return false
	}
	row, ok, err := r.cursor.nextRow()
	if err != nil {
		// A mid-drain streaming error (a 54P01 cost abort, a canceled ctx surfaced by the meter, or an
		// arithmetic trap): stop iteration; Err() surfaces it (streaming.md §6). setErr also aborts an
		// open block (attachBlockPoison) — PG aborts a transaction on a drain-time statement error too.
		r.setErr(err)
		return false
	}
	if !ok {
		return false
	}
	r.current = row
	r.valid = true
	return true
}

// Row returns the current row (valid after a Next that returned true).
func (r *Rows) Row() []Value { return r.current }

// ColumnNames are the output column names of the query result.
func (r *Rows) ColumnNames() []string { return r.columnNames }

// Cost is the deterministic execution cost accrued by the query (CLAUDE.md §13).
func (r *Rows) Cost() int64 { return r.cursor.costAccrued() }

// RowsAffected reports the command tag carried on a Rows: how many rows a DML statement
// (INSERT/UPDATE/DELETE without RETURNING) touched; ok is false for a SELECT / DDL / transaction
// control, which have no count (spec/design/api.md §4). This is how the exec-side path (Exec) surfaces
// the tag after draining the result set — a statement run through the total query seam is a Rows with
// no columns whose tag lives here.
func (r *Rows) RowsAffected() (n int64, ok bool) { return r.rowsAffected, r.hasAffected }
