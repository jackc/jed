package jed

// Host API surface for the Go core (spec/design/api.md §1): prepare a statement, execute or
// query it (with optional $N bind parameters), and iterate result rows. Thin wrappers over the
// parser + executor — the conformance contract still binds (the executor is unchanged).

import "context"

// AccessMode selects whether a transaction may write.
type AccessMode uint8

// Transaction access modes, for [TxOptions].
const (
	// ReadWrite transactions may read and write. It is the zero value, PostgreSQL's default.
	// Beginning one waits for the database's single writer slot.
	ReadWrite AccessMode = iota
	// ReadOnly transactions read one consistent snapshot and never wait for a writer. A write
	// inside fails with SQLSTATE 25006.
	ReadOnly
)

// TxOptions configures a transaction started by Begin or BeginFunc. The zero value is a
// read-write transaction.
type TxOptions struct {
	// AccessMode is ReadWrite (the default) or ReadOnly. Any other value makes Begin fail with
	// SQLSTATE 22023.
	AccessMode AccessMode
}

func (o TxOptions) writable() (bool, error) {
	switch o.AccessMode {
	case ReadWrite:
		return true, nil
	case ReadOnly:
		return false, nil
	}
	return false, newError(InvalidParameterValue, "invalid parameter value: unknown transaction access mode")
}

// Transaction is an open transaction, started by Begin or handed to a BeginFunc callback.
// Run statements through its Query and Exec methods and end it with Commit or Rollback.
// A Transaction must not be used from more than one goroutine at a time.
//
// Transaction control belongs to the Transaction: running COMMIT or ROLLBACK as SQL through it
// fails with SQLSTATE 2D000. SAVEPOINT, RELEASE and ROLLBACK TO work as in PostgreSQL.
type Transaction struct {
	db *engine
	// sess is the session whose block this transaction is; nil for an engine-level transaction
	// (the white-box tests' engine.Begin).
	sess *Session
	// owned marks a transaction whose session was minted for it (Database.Begin): ending the
	// transaction closes the session.
	owned bool
	done  bool
	// scoped marks a Transaction handed to a BeginFunc callback (Session.withBlock): the callback IS
	// the block's lifetime, so ending it from inside — SQL COMMIT/ROLLBACK or this handle's
	// Commit/Rollback — is 2D000 invalid_transaction_termination (spec/design/api.md §2.2), never a
	// silent discard of the callback's writes.
	scoped bool
}

// queryValues is the unexported raw (sql, []Value) -> *Rows seam the ergonomic Query/Exec/QueryRow
// (ergonomic.go) build on within this transaction. Total: a non-query statement (a write in a READ
// ONLY transaction is 25006; a statement error aborts the block, every later statement but
// commit/rollback then 25P02) returns a no-column cursor carrying the command tag. A parse error
// (42601 / the 54000 input-size limit) is a statement error too, so it aborts the block
// (transactions.md §6 — PostgreSQL aborts on ANY error, the parse stage included).
func (tx *Transaction) queryValues(sql string, params []Value) (*Rows, error) {
	if err := tx.checkOpen(); err != nil {
		return nil, err
	}
	stmt, err := tx.db.parse(sql)
	if err != nil {
		tx.db.failOpenBlock()
		return nil, err
	}
	return tx.queryStmt(stmt, params, nil, nil)
}

// queryStmt routes a parsed statement through the engine's lazy lanes after the transaction-control
// gate: through a session-backed Transaction SQL COMMIT/ROLLBACK is 2D000 and aborts the block
// (api.md §2.2 — the PostgreSQL rule for COMMIT inside a procedure called within a transaction
// block; the Transaction's later statements would otherwise run outside the block, and a callback's
// writes be discarded). A nested BEGIN stays the engine's 25001 / 25P02 (transactions.md §4.2), and
// the SAVEPOINT family runs on the open block as api.md §2.2 specifies.
func (tx *Transaction) queryStmt(stmt statement, params []Value, sc *stmtCache, ic *insertStmtCache) (*Rows, error) {
	if err := tx.checkOpen(); err != nil {
		return nil, err
	}
	if (tx.scoped || tx.sess != nil) && (stmt.Commit != nil || stmt.Rollback != nil) {
		tx.db.failOpenBlock()
		return nil, errInvalidTransactionTermination()
	}
	return tx.db.queryStmt(stmt, params, sc, ic)
}

// checkOpen rejects a statement through a session-backed Transaction that has already ended: it
// would otherwise run on the session in autocommit, outside the transaction its caller meant.
func (tx *Transaction) checkOpen() error {
	if tx.done && !tx.scoped && tx.sess != nil {
		return newError(NoActiveSQLTransaction, "the transaction has already ended")
	}
	return nil
}

// errInvalidTransactionTermination is 2D000: a Transaction's block ends only through its own
// Commit/Rollback, and a BeginFunc callback's only when the callback returns.
func errInvalidTransactionTermination() error {
	return newError(InvalidTransactionTermination, "invalid transaction termination")
}

// Render formats v as PostgreSQL displays a result column, rendering timestamptz values in the
// transaction's session time zone, as [Session.Render] does.
func (tx *Transaction) Render(v Value) (string, error) {
	return v.renderIn(tx.db.session.timeZone)
}

// Commit commits the transaction. A canceled ctx rolls it back instead and returns the
// cancellation error; once commit work starts it is not canceled. If a statement in the
// transaction failed, Commit rolls back and returns SQLSTATE 25P02, so a failed transaction
// never looks committed. Commit on a transaction that has already ended returns nil.
// Inside a BeginFunc callback it fails with SQLSTATE 2D000: return from the callback instead.
func (tx *Transaction) Commit(ctx context.Context) error {
	if tx.scoped {
		return errInvalidTransactionTermination() // the callback ends the block (api.md §2.2)
	}
	if tx.done {
		return nil
	}
	tx.done = true
	if tx.sess == nil {
		return tx.db.Commit()
	}
	defer tx.release()
	return tx.sess.finishCallerBlock(ctx)
}

// Rollback rolls the transaction back. It returns nil on a transaction that has already ended,
// so deferring it after Begin is safe. ctx is accepted for symmetry with Commit; a rollback
// never waits. Inside a BeginFunc callback it fails with SQLSTATE 2D000.
func (tx *Transaction) Rollback(ctx context.Context) error {
	if tx.scoped {
		return errInvalidTransactionTermination() // the callback ends the block (api.md §2.2)
	}
	if tx.done {
		return nil
	}
	tx.done = true
	if tx.sess == nil {
		return tx.db.Rollback()
	}
	defer tx.release()
	return tx.sess.rollbackBlock()
}

// release closes the session a Database.Begin minted for this transaction.
func (tx *Transaction) release() {
	if tx.owned {
		_ = tx.sess.Close()
	}
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
	defer func() { _ = tx.Rollback(context.Background()) }()
	if err := fn(tx); err != nil {
		return err
	}
	return tx.Commit(context.Background())
}

// PreparedStatement is a statement parsed once by Prepare and run any number of times. It is not
// tied to a session: run it with the QueryPrepared, ExecPrepared or QueryRowPrepared method of a
// [Database], [Session] or [Transaction], which supplies the session it runs in (its privileges,
// snapshot and temporary tables). A PreparedStatement may be shared across sessions and
// goroutines. Its query plan is cached and reused until a schema change requires planning again.
type PreparedStatement struct {
	ast statement
	// sc memoizes the resolved scan plan across executes so a repeated query skips planning (the
	// plan cache, spec/design/api.md §2.4). Populated lazily on the first cacheable query execute;
	// an entry is keyed to the Database (core) + committed catalog generation it was resolved
	// against, so DDL — or executing on a different database — re-plans. Zero value is empty.
	// Query-only — a write / materialized shape never touches it.
	sc stmtCache
	// ic is the separately typed prepared-INSERT slot. The AST kind is fixed, so a statement can
	// populate sc or ic, never both (spec/design/api.md §2.4).
	ic insertStmtCache
}

// queryValues is a one-shot: parse + run a statement, binding params, returning a row cursor. A
// single-table no-blocking-operator read is served by a lazy STREAMING cursor (spec/design/streaming.md
// §4, S3); a blocking read (ORDER BY/DISTINCT/aggregate/window/join) by a lazy BUFFERED cursor (S4)
// that buffers the input but yields the output one row at a time. Both pull over a pinned snapshot with
// bounded peak output memory and a caller early-exit; a top-level set operation / pure-query WITH is
// served by a lazy DEFERRED cursor (streaming.md §7) that defers the run to the first pull and yields
// the result one row at a time. Total: a non-query statement returns a no-column cursor carrying the
// command tag. (This is the bare single-handle engine; the watermark pin lives on the shared-core
// Session.queryValues path.)
func (db *engine) queryValues(sql string, params []Value) (*Rows, error) {
	stmt, err := db.parse(sql)
	if err != nil {
		return nil, err
	}
	return db.queryStmt(stmt, params, nil, nil) // one-shot: no cross-call plan cache (still plans once)
}

// queryStmt routes an already-parsed query AST through the lazy scan (streaming/buffered) then
// deferred lanes, planning a scan-shaped SELECT exactly once (tryScanQuery), and falling back to the
// materialized ExecuteStmtParams for a shape no lazy lane covers (a write, a nextval/setval SELECT, a
// data-modifying WITH). Shared by queryValues (parse-then-route, sc nil) and a prepared query
// (the *Prepared methods pass the statement's stmtCache), so a prepared query streams identically to
// an ad-hoc one but reuses its cached plan across executes. (This is the bare single-handle engine;
// the watermark pin lives on the shared-core Session.queryValues path.)
func (db *engine) queryStmt(stmt statement, params []Value, sc *stmtCache, ic *insertStmtCache) (*Rows, error) {
	// A read served by a lazy lane skips the materialized dispatch, so enforce the read-path admission
	// gates (25P02 / 54P02 / 42501) up front — reads only (transaction control must work in a failed
	// block; a write is gated inside dispatch on the fall-through). Keeps the bare-engine queryValues a
	// safe total seam like the shared-core Session.queryValues (executor.go gateReadLanes, CLAUDE.md §13).
	if stmt.Begin == nil && stmt.Commit == nil && stmt.Rollback == nil && stmt.Savepoint == nil && !db.stmtIsWriteWithViews(stmt) {
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
	out, err := db.executeStmtParamsCached(stmt, params, ic)
	if err != nil {
		return nil, err
	}
	return rowsFromOutcome(out, db.session.queryAccount()), nil
}

// Rows is the result of Query: a statement's rows, read one at a time, with the column names and
// types, the command tag and the execution cost. Rows are produced as they are read, so a large
// result need not fit in memory. A statement that returns no rows yields a Rows with no columns
// whose command tag is in [Rows.RowsAffected].
//
// Read the rows with [Rows.Next] and [Rows.Scan], or with [Rows.All] or [MapRows], then check
// [Rows.Err]. Call [Rows.Close] if you stop before the end:
//
//	rows, err := db.Query(ctx, "SELECT id, name FROM users")
//	if err != nil {
//		return err
//	}
//	defer rows.Close()
//	for rows.Next() {
//		var id int64
//		var name string
//		if err := rows.Scan(&id, &name); err != nil {
//			return err
//		}
//	}
//	return rows.Err()
type Rows struct {
	// executionComplete marks materialized results, whose execution and any commit
	// finished before Query returned. Later cancellation cannot undo that success.
	executionComplete bool
	columnNames       []string
	columnTypes       []string
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
	// Session.queryValues for a streaming cursor, called by Close (Go has no destructor; the ergonomic
	// iterators close on loop exit). nil for a buffered cursor or a bare single-handle stream.
	onClose func()
	// current is the row the last successful Next produced; valid reports whether there is one.
	current []Value
	valid   bool
	closed  bool
	// done is captured at Query time for the entry check. The cursor's snapshot and
	// meters independently retain the same channel throughout execution inside Next.
	done <-chan struct{}
	// cancelErr is retained only for cancelable operations, to preserve the Go
	// sentinel when a lazy cursor observes cancellation. Close releases it.
	cancelErr func() error
	// err holds a terminal error reached during iteration (cancellation or a mid-stream
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
func rowsFromOutcome(out outcome, acct queryAccount) *Rows {
	return &Rows{
		executionComplete: true,
		columnNames:       out.ColumnNames,
		columnTypes:       out.ColumnTypes,
		cursor:            bufferedCursor(out.Rows, out.Cost, acct),
		rowsAffected:      out.RowsAffected,
		hasAffected:       out.HasRowsAffected,
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
	err = cancellationCause(err, r.cancelErr)
	r.err = err
	if r.onErr != nil {
		r.onErr(err)
		r.onErr = nil
	}
}

// Next advances to the next row and reports whether there is one. It returns false when the rows
// run out, when producing a row fails, or when the query's context is canceled while rows are
// still being produced; [Rows.Err] tells which. Next closes the Rows when it returns false.
func (r *Rows) Next() bool {
	r.valid = false
	if r.closed || r.err != nil {
		return false
	}
	if err := doneErr(r.done); err != nil {
		r.setErr(err)
		r.Close()
		return false
	}
	row, ok, err := r.cursor.nextRow()
	if err != nil {
		// A mid-drain streaming error (a 54P01 cost abort, a canceled ctx surfaced by the meter, or an
		// arithmetic trap): stop iteration; Err() surfaces it (streaming.md §6). setErr also aborts an
		// open block (attachBlockPoison) — PG aborts a transaction on a drain-time statement error too.
		r.setErr(err)
		r.Close()
		return false
	}
	if !ok {
		r.Close()
		return false
	}
	r.current = row
	r.valid = true
	return true
}

// Row returns the current row's values. It is meaningful only after [Rows.Next] returns true.
func (r *Rows) Row() []Value { return r.current }

// ColumnNames returns the names of the result columns. A statement that returns no rows has
// none.
func (r *Rows) ColumnNames() []string { return r.columnNames }

// Cost returns the execution cost the statement has accrued so far; once every row has been read,
// it is the statement's total. Cost is deterministic: the same statement on the same data always
// costs the same.
func (r *Rows) Cost() int64 { return r.cursor.costAccrued() }

// RowsAffected returns the number of rows an INSERT, UPDATE, DELETE or MERGE without RETURNING
// affected. ok is false for a statement that carries no count, such as a query, DDL or
// transaction control. Exec returns the same count in its [Result].
func (r *Rows) RowsAffected() (n int64, ok bool) { return r.rowsAffected, r.hasAffected }
