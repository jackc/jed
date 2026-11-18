package jed

// Ergonomic host bindings (spec/design/api.md §11) — a pgx-style layer over the typed Value
// surface. Query / Exec / QueryRow are the sole public query surface across every handle
// (*Database, *Session, *Transaction, *PreparedStatement); the raw (sql, []Value) seam is the
// unexported queryValues they build on. Layers of increasing fidelity, one []Value currency
// underneath:
//
//   - Query / Exec / QueryRow take plain Go args (...any) and a context.Context, and return a
//     cursor whose Scan converts into Go-native destinations. This is the default surface.
//   - Typed accessors (Rows.Int/Text/...) skip the Scan type switch for hot loops.
//   - Full fidelity needs no separate method: a raw jed.Value arg passes straight through the args
//     (toValue's Value case), and Rows.Value returns a column as its raw engine Value — so a rich
//     type with no clean native counterpart (a range, a jsonb, a composite) round-trips losslessly
//     without leaving the Query/Exec/QueryRow surface.
//
// Cancellation is wired through the cost meter: each Query/Exec/QueryRow arms a poll on the engine
// that runs the statement (armCancel), and the meter's execution checkpoints consult it, so a flipped
// context.Context aborts a long-running statement with 57014 at the next execution checkpoint — not only
// at the cursor boundary (api.md §11.4). The 57014 query_canceled SQLSTATE is registered in
// spec/errors/registry.toml. Container types in Values still degrade to the raw Value (the §11.5
// follow-up), and the same surface on the shared Read/Write handles is pending.
//
// The API is a per-impl surface, NOT the shared conformance corpus (api.md §1) — so this lands
// without touching the contract; the Rust/TS cores mirror the SHAPE idiomatically (api.md §11).

import (
	"context"
	"errors"
	"fmt"
	"iter"
	"math"
	"reflect"
	"slices"
	"strings"
	"time"
)

// ErrNoRows is returned by [Row.Scan] when the query produced no rows. Test for it with errors.Is.
var ErrNoRows = errors.New("jed: no rows in result set")

// Valuer is implemented by types that convert themselves to a query parameter. Query, Exec and
// QueryRow call JedValue on any argument that implements it and bind the returned [Value].
type Valuer interface{ JedValue() (Value, error) }

// Scanner is implemented by scan destinations that convert a column value themselves.
// [Rows.Scan], [Row.Scan] and [Value.Scan] call ScanJed with the column's value, which may be
// NULL.
type Scanner interface{ ScanJed(v Value) error }

// Queryer is implemented by *Database, *Session and *Transaction, so a helper can run statements
// on a database, a session or an open transaction interchangeably. The *Prepared methods run a
// [PreparedStatement] in the session the receiver supplies.
//
// Applications should usually define an interface in their own package with only the methods
// they need. Queryer is a convenience for helpers that use the full set.
type Queryer interface {
	Query(ctx context.Context, sql string, args ...any) (*Rows, error)
	QueryRow(ctx context.Context, sql string, args ...any) *Row
	Exec(ctx context.Context, sql string, args ...any) (Result, error)
	QueryPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) (*Rows, error)
	QueryRowPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) *Row
	ExecPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) (Result, error)
}

var (
	_ Queryer = (*Database)(nil)
	_ Queryer = (*Session)(nil)
	_ Queryer = (*Transaction)(nil)
)

// ───────────────────────────── Query / Exec / QueryRow ─────────────────────────────
//
// The same three ergonomic methods sit on *Database (autocommit), *Session (durable), and
// *Transaction (inside an explicit block), with *Prepared siblings taking a PreparedStatement in
// place of sql. Each converts native args and arms cancellation on the engine that runs the
// statement, so the conversion + cancellation logic lives once, in ergoQuery/ergoExec; the per-type
// methods are one-liners that supply the right engine + raw []Value primitive. armCancel threads ctx
// into the statement's cost meter (api.md §11.4), so a
// flipped context aborts a long-running statement at the executor's cancellation checkpoint — not only
// at the cursor boundary (doneErr, the cheap pre-exec / cursor poll).

func ergoQuery(ctx context.Context, eng *engine, args []any, raw func([]Value) (*Rows, error)) (*Rows, error) {
	params, err := toValues(args)
	if err != nil {
		eng.failOpenBlock() // an arg-conversion error is a statement error: it aborts an open block (transactions.md §6)
		return nil, err
	}
	done := contextDone(ctx)
	if err := doneErr(done); err != nil {
		eng.failOpenBlock() // so is a pre-execution 57014 (api.md §11.4)
		return nil, contextError(ctx, err)
	}
	defer eng.restoreCancel(eng.armCancel(done))
	rows, err := raw(params)
	if err != nil {
		return nil, contextError(ctx, err)
	}
	if !rows.executionComplete {
		rows.done = done
	}
	if rows.done != nil {
		rows.cancelErr = ctx.Err
	}
	return rows, nil
}

// ergoExec runs a statement through the same total query seam as ergoQuery and returns its command
// tag — "Exec is simply throw away any result set". It drains the Rows (a write is a buffered empty
// cursor, so the drain is trivial; a SELECT run through Exec streams and is discarded in O(1) peak
// memory), which on Close releases any reader-liveness pin the streaming path registered (Go has no
// destructor — the deferred Close is load-bearing, spec/design/streaming.md §5). The affected count +
// accrued cost are then read off the drained Rows. A mid-drain fault (a 54P01 cost abort, a 57014
// cancellation, an arithmetic trap) surfaces from rows.Err(); a pre-drain error (e.g. a 23505 write
// conflict) surfaces from ergoQuery. Cancellation is armed once, in ergoQuery, so it is not repeated
// here (spec/design/api.md §11).
func ergoExec(ctx context.Context, eng *engine, args []any, raw func([]Value) (*Rows, error)) (Result, error) {
	rows, err := ergoQuery(ctx, eng, args, raw)
	if err != nil {
		return Result{}, err
	}
	defer rows.Close()
	for rows.Next() {
	}
	if err := rows.Err(); err != nil {
		return Result{}, err
	}
	n, ok := rows.RowsAffected()
	return Result{rowsAffected: n, hasAffected: ok, cost: rows.Cost()}, nil
}

// Query runs sql in a new autocommit session and returns its rows. args bind to $1, $2, … in
// order. An argument may be nil (NULL), a bool, any integer type (a uint or uint64 above
// math.MaxInt64 is an error), float32, float64, a string (which must be valid UTF-8 without NUL
// bytes, else SQLSTATE 22021), []byte, [Decimal], [Interval], time.Time, a [Value], a [Valuer],
// or any other slice, which binds as a one-dimensional array (a nil slice is NULL). A time.Time
// binds as timestamptz; where the parameter is a timestamp or date, its calendar fields in its
// own location are used.
//
// A statement that returns no rows is valid: the Rows has no columns and carries the command tag
// in [Rows.RowsAffected]. The caller must Close the Rows, or iterate it to the end, and then check
// [Rows.Err]. If ctx is canceled before or during execution, the statement fails with SQLSTATE
// 57014 and the error wraps ctx.Err().
func (db *Database) Query(ctx context.Context, sql string, args ...any) (*Rows, error) {
	s := db.sessionWithState(newSession(db.core.env))
	defer s.Close()
	return ergoQuery(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.queryValues(sql, p) })
}

// Exec runs sql in a new autocommit session and returns its command tag. args bind as in
// [Database.Query]. A query run through Exec is valid; its rows are read and discarded. If ctx is
// canceled before or during execution, the statement fails with SQLSTATE 57014.
func (db *Database) Exec(ctx context.Context, sql string, args ...any) (Result, error) {
	s := db.sessionWithState(newSession(db.core.env))
	defer s.Close()
	return ergoExec(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.queryValues(sql, p) })
}

// QueryRow runs sql in a new autocommit session and returns a [Row] holding its first row. args
// bind as in [Database.Query]. Any error is deferred to [Row.Scan], which returns [ErrNoRows] when
// the query produced no rows.
func (db *Database) QueryRow(ctx context.Context, sql string, args ...any) *Row {
	rows, err := db.Query(ctx, sql, args...)
	return &Row{rows: rows, err: err}
}

// Query runs sql inside the transaction and returns its rows. args bind as in [Database.Query].
// The caller must Close the Rows, or iterate it to the end, and then check [Rows.Err].
//
// An error, including one reached while iterating the Rows or a canceled ctx (SQLSTATE 57014),
// aborts the transaction: later statements fail with SQLSTATE 25P02 until it is rolled back or
// rolled back to a savepoint. COMMIT or ROLLBACK run as SQL fails with SQLSTATE 2D000; use
// [Transaction.Commit] or [Transaction.Rollback]. After the transaction has ended, Query fails
// with SQLSTATE 25P01.
func (tx *Transaction) Query(ctx context.Context, sql string, args ...any) (*Rows, error) {
	return ergoQuery(ctx, tx.db, args, func(p []Value) (*Rows, error) { return tx.queryValues(sql, p) })
}

// Exec runs sql inside the transaction and returns its command tag. args bind as in
// [Database.Query], and errors abort the transaction as described for [Transaction.Query]. A
// query run through Exec is valid; its rows are read and discarded.
func (tx *Transaction) Exec(ctx context.Context, sql string, args ...any) (Result, error) {
	return ergoExec(ctx, tx.db, args, func(p []Value) (*Rows, error) { return tx.queryValues(sql, p) })
}

// QueryRow runs sql inside the transaction and returns a [Row] holding its first row. args bind
// as in [Database.Query]. Any error is deferred to [Row.Scan], which returns [ErrNoRows] when the
// query produced no rows.
func (tx *Transaction) QueryRow(ctx context.Context, sql string, args ...any) *Row {
	rows, err := tx.Query(ctx, sql, args...)
	return &Row{rows: rows, err: err}
}

// QueryPrepared runs stmt in a new autocommit session and returns its rows, as [Database.Query]
// does for SQL text.
func (db *Database) QueryPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) (*Rows, error) {
	s := db.sessionWithState(newSession(db.core.env))
	defer s.Close()
	return ergoQuery(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.queryStmt(stmt.ast, p, &stmt.sc, &stmt.ic) })
}

// ExecPrepared runs stmt in a new autocommit session and returns its command tag, as
// [Database.Exec] does for SQL text.
func (db *Database) ExecPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) (Result, error) {
	s := db.sessionWithState(newSession(db.core.env))
	defer s.Close()
	return ergoExec(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.queryStmt(stmt.ast, p, &stmt.sc, &stmt.ic) })
}

// QueryRowPrepared runs stmt in a new autocommit session and returns a [Row] holding its first
// row, as [Database.QueryRow] does for SQL text.
func (db *Database) QueryRowPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) *Row {
	rows, err := db.QueryPrepared(ctx, stmt, args...)
	return &Row{rows: rows, err: err}
}

// QueryPrepared runs stmt on the session and returns its rows, as [Session.Query] does for SQL
// text.
func (s *Session) QueryPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) (*Rows, error) {
	return ergoQuery(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.queryStmt(stmt.ast, p, &stmt.sc, &stmt.ic) })
}

// ExecPrepared runs stmt on the session and returns its command tag, as [Session.Exec] does for
// SQL text.
func (s *Session) ExecPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) (Result, error) {
	return ergoExec(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.queryStmt(stmt.ast, p, &stmt.sc, &stmt.ic) })
}

// QueryRowPrepared runs stmt on the session and returns a [Row] holding its first row, as
// [Session.QueryRow] does for SQL text.
func (s *Session) QueryRowPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) *Row {
	rows, err := s.QueryPrepared(ctx, stmt, args...)
	return &Row{rows: rows, err: err}
}

// QueryPrepared runs stmt inside the transaction and returns its rows, as [Transaction.Query] does
// for SQL text.
func (tx *Transaction) QueryPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) (*Rows, error) {
	return ergoQuery(ctx, tx.db, args, func(p []Value) (*Rows, error) { return tx.queryStmt(stmt.ast, p, &stmt.sc, &stmt.ic) })
}

// ExecPrepared runs stmt inside the transaction and returns its command tag, as
// [Transaction.Exec] does for SQL text.
func (tx *Transaction) ExecPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) (Result, error) {
	return ergoExec(ctx, tx.db, args, func(p []Value) (*Rows, error) { return tx.queryStmt(stmt.ast, p, &stmt.sc, &stmt.ic) })
}

// QueryRowPrepared runs stmt inside the transaction and returns a [Row] holding its first row, as
// [Transaction.QueryRow] does for SQL text.
func (tx *Transaction) QueryRowPrepared(ctx context.Context, stmt *PreparedStatement, args ...any) *Row {
	rows, err := tx.QueryPrepared(ctx, stmt, args...)
	return &Row{rows: rows, err: err}
}

// Query runs sql on the session and returns its rows. args bind as in [Database.Query]. Unlike
// [Database.Query], which uses a new session per call, the session's state persists across
// calls: an open transaction, settings, sequence currval and temporary tables. The caller must
// Close the Rows, or iterate it to the end, and then check [Rows.Err]. If ctx is canceled before
// or during execution, the statement fails with SQLSTATE 57014 and the error wraps ctx.Err().
func (s *Session) Query(ctx context.Context, sql string, args ...any) (*Rows, error) {
	return ergoQuery(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.queryValues(sql, p) })
}

// Exec runs sql on the session and returns its command tag. args bind as in [Database.Query]. A
// query run through Exec is valid; its rows are read and discarded.
func (s *Session) Exec(ctx context.Context, sql string, args ...any) (Result, error) {
	return ergoExec(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.queryValues(sql, p) })
}

// QueryRow runs sql on the session and returns a [Row] holding its first row. args bind as in
// [Database.Query]. Any error is deferred to [Row.Scan], which returns [ErrNoRows] when the query
// produced no rows.
func (s *Session) QueryRow(ctx context.Context, sql string, args ...any) *Row {
	rows, err := s.Query(ctx, sql, args...)
	return &Row{rows: rows, err: err}
}

// ───────────────────────────── Result (command tag) ─────────────────────────────

// Result is the command tag returned by Exec: the affected-row count and the execution cost.
type Result struct {
	rowsAffected int64
	hasAffected  bool
	cost         int64
}

// RowsAffected returns the number of rows an INSERT, UPDATE, DELETE or MERGE affected. ok is false
// for a statement that carries no count, such as a query, DDL or transaction control.
func (r Result) RowsAffected() (n int64, ok bool) { return r.rowsAffected, r.hasAffected }

// Cost returns the execution cost the statement accrued. Cost is deterministic: the same statement
// on the same data always costs the same.
func (r Result) Cost() int64 { return r.cost }

// ───────────────────────────── Row (single-row) ─────────────────────────────

// Row is the result of QueryRow. Read it with [Row.Scan].
type Row struct {
	rows *Rows
	err  error
}

// Scan copies the first row's columns into dest, as [Rows.Scan] does, and releases the result. It
// returns [ErrNoRows] if the query produced no rows, or the query's error if it failed. Rows after
// the first are ignored.
func (row *Row) Scan(dest ...any) error {
	if row.err != nil {
		return row.err
	}
	defer row.rows.Close()
	if !row.rows.Next() {
		if err := row.rows.Err(); err != nil {
			return err
		}
		return ErrNoRows
	}
	return row.rows.Scan(dest...)
}

// ───────────────────────────── Rows: Scan / Values / Err / Close ─────────────────────────────

// Scan copies the current row's columns into dest, one pointer per column. Supported
// destinations are *int64, *int, *int32 and *int16 (an out-of-range value is an error),
// *float64 and *float32 (from either float type or an integer), *bool, *string (text), *[]byte
// (bytea or uuid), *[Decimal], *[Interval], *time.Time (a finite timestamp, timestamptz or date,
// in UTC), *[ArrayVal], *[RangeVal], *[JSONNode] (jsonb), *[]Value (a composite's fields),
// *[Value] (the raw value), *any (as [Rows.Values] converts it), and any [Scanner]. Scanning NULL
// into any other destination is an error; use *Null[T], *any, *Value or a Scanner to accept
// NULL.
//
// Scan itself does not allocate, apart from the copies it makes for *[]byte and the container
// destinations. Call it only after [Rows.Next] returns true.
func (r *Rows) Scan(dest ...any) error {
	if !r.valid {
		return errors.New("jed: Scan called without a successful Next")
	}
	row := r.current
	if len(dest) != len(row) {
		return fmt.Errorf("jed: Scan got %d destination(s) for a %d-column row", len(dest), len(row))
	}
	for i, d := range dest {
		if err := scanOne(row[i], d); err != nil {
			return fmt.Errorf("jed: scanning column %d (%s): %w", i, r.colName(i), err)
		}
	}
	return nil
}

// Values returns the current row as Go values, for callers that do not know the result's columns
// in advance. NULL is nil; integers are int64; floats are float32 or float64; bool, text and
// bytea are bool, string and []byte; a uuid is its canonical string; decimal and interval are
// [Decimal] and [Interval]; a finite timestamp, timestamptz or date is a time.Time in UTC. Any
// other value, including an infinite timestamp, array, range, composite or json, is its [Value].
func (r *Rows) Values() ([]any, error) {
	if !r.valid {
		return nil, errors.New("jed: Values called without a successful Next")
	}
	row := r.current
	out := make([]any, len(row))
	for i, v := range row {
		out[i] = valueToAny(v)
	}
	return out, nil
}

// Err returns the error, if any, that ended iteration early, such as a canceled context (SQLSTATE
// 57014) or a statement error raised while rows were being produced. Check it after the loop.
func (r *Rows) Err() error { return r.err }

// Close releases the Rows and the database snapshot it reads from. Next closes the Rows when the
// rows run out or an error occurs, and [Rows.All], [MapRows] and [Row.Scan] close it when they
// finish, but a caller that stops reading early must call Close; an open Rows keeps old data from
// being reclaimed. Close is idempotent and always returns nil.
func (r *Rows) Close() error {
	if r.closed {
		return nil
	}
	r.closed = true
	r.valid = false
	r.done = nil
	r.cancelErr = nil
	r.onErr = nil
	r.cursor.close()
	if r.onClose != nil {
		r.onClose()
		r.onClose = nil
	}
	return nil
}

// ColumnTypes returns the type name of each result column, in the same order as
// [Rows.ColumnNames].
func (r *Rows) ColumnTypes() []string { return r.columnTypes }

// ───────────────────────────── Rows: typed fast path ─────────────────────────────

// Int returns column col (0-based) of the current row as an int64. It fails if the value is not an
// integer, including NULL. The typed accessors are a faster alternative to [Rows.Scan] in hot
// loops.
func (r *Rows) Int(col int) (int64, error) {
	v, err := r.col(col)
	if err != nil {
		return 0, err
	}
	if v.kind != ValInt {
		return 0, typeErr(v, "int")
	}
	return v.num, nil
}

// Text returns column col of the current row as a string. It fails if the value is not text,
// including NULL.
func (r *Rows) Text(col int) (string, error) {
	v, err := r.col(col)
	if err != nil {
		return "", err
	}
	if v.kind != ValText {
		return "", typeErr(v, "text")
	}
	return v.str(), nil
}

// Bool returns column col of the current row as a bool. It fails if the value is not a boolean,
// including NULL.
func (r *Rows) Bool(col int) (bool, error) {
	v, err := r.col(col)
	if err != nil {
		return false, err
	}
	if v.kind != ValBool {
		return false, typeErr(v, "bool")
	}
	return v.boolVal(), nil
}

// Float returns column col of the current row as a float64, converting from either float type or
// an integer. It fails for any other value, including NULL.
func (r *Rows) Float(col int) (float64, error) {
	v, err := r.col(col)
	if err != nil {
		return 0, err
	}
	return asFloat(v)
}

// Bytes returns a copy of column col of the current row, a bytea or uuid, as a []byte. It fails
// for any other value, including NULL.
func (r *Rows) Bytes(col int) ([]byte, error) {
	v, err := r.col(col)
	if err != nil {
		return nil, err
	}
	if v.kind != ValBytea && v.kind != ValUUID {
		return nil, typeErr(v, "bytea")
	}
	return []byte(v.str()), nil
}

// IsNull reports whether column col of the current row is SQL NULL.
func (r *Rows) IsNull(col int) bool {
	v, err := r.col(col)
	return err == nil && v.kind == ValNull
}

// Value returns column col of the current row as a [Value], losing nothing. It returns a NULL
// Value if col is out of range or there is no current row.
func (r *Rows) Value(col int) Value {
	v, err := r.col(col)
	if err != nil {
		return NullValue()
	}
	return v
}

func (r *Rows) col(col int) (Value, error) {
	if !r.valid {
		return Value{}, errors.New("jed: column access without a successful Next")
	}
	row := r.current
	if col < 0 || col >= len(row) {
		return Value{}, fmt.Errorf("jed: column %d out of range (row has %d)", col, len(row))
	}
	return row[col], nil
}

func (r *Rows) colName(i int) string {
	if i >= 0 && i < len(r.columnNames) {
		return r.columnNames[i]
	}
	return fmt.Sprintf("col%d", i)
}

// ───────────────────────────── Iterators (Go 1.23+) ─────────────────────────────

// All returns a single-use iterator over the remaining rows. Each iteration yields r positioned at
// the next row; read it with [Rows.Scan] or the typed accessors. The iterator closes r when the
// loop ends for any reason (exhaustion, break, return or panic), so no deferred Close is needed.
// Check [Rows.Err] after the loop.
func (r *Rows) All() iter.Seq[*Rows] {
	return func(yield func(*Rows) bool) {
		defer r.Close()
		for r.Next() {
			if !yield(r) {
				return
			}
		}
	}
}

// MapRows returns a single-use iterator that maps each remaining row through fn and yields the
// result with fn's error. Iteration stops after the first error from fn. An error that ends the
// rows early, such as a canceled context, is yielded last as a zero value with the error.
//
// Once iteration begins, MapRows closes rows when the loop ends for any reason. If the iterator is
// never used, the caller must close rows. Because it returns an iter.Seq2[T, error], it cannot be
// passed to slices.Collect, which takes an iter.Seq[T]; handle each error in the loop. Pair it with
// [RowToStructByName] to map rows to structs:
//
//	for u, err := range jed.MapRows(rows, jed.RowToStructByName[User]) { ... }
func MapRows[T any](rows *Rows, fn func(*Rows) (T, error)) iter.Seq2[T, error] {
	return func(yield func(T, error) bool) {
		defer rows.Close()
		for rows.Next() {
			v, err := fn(rows)
			if !yield(v, err) {
				return
			}
			if err != nil {
				return
			}
		}
		if err := rows.Err(); err != nil {
			var zero T
			yield(zero, err)
		}
	}
}

// ───────────────────────────── Generic row mappers ─────────────────────────────

// RowTo returns the current row's only column as a T, converted as [Rows.Scan] converts into a *T.
// It fails if the row does not have exactly one column. Use it with [MapRows], or for a query such
// as SELECT count(*).
func RowTo[T any](rows *Rows) (T, error) {
	var out T
	if !rows.valid {
		return out, errors.New("jed: RowTo without a successful Next")
	}
	row := rows.current
	if len(row) != 1 {
		return out, fmt.Errorf("jed: RowTo expected 1 column, got %d", len(row))
	}
	err := assignValue(row[0], &out)
	return out, err
}

// RowToStructByName returns the current row as a struct T, matching each column name,
// case-insensitively, to an exported field's `db:"name"` tag or, without a tag, its field name. A
// `db:"-"` tag excludes a field. It fails if a column has no matching field; fields with no
// matching column keep their zero value. It uses reflection on every row, so prefer [Rows.Scan]
// or the typed accessors in hot loops.
func RowToStructByName[T any](rows *Rows) (T, error) {
	var out T
	if !rows.valid {
		return out, errors.New("jed: RowToStructByName without a successful Next")
	}
	rv := reflect.ValueOf(&out).Elem()
	if rv.Kind() != reflect.Struct {
		return out, fmt.Errorf("jed: RowToStructByName needs a struct, got %s", rv.Kind())
	}
	rt := rv.Type()
	byName := make(map[string]int, rt.NumField())
	for i := 0; i < rt.NumField(); i++ {
		f := rt.Field(i)
		if f.PkgPath != "" { // unexported
			continue
		}
		tag := f.Tag.Get("db")
		if tag == "-" {
			continue
		}
		name := f.Name
		if tag != "" {
			name = strings.Split(tag, ",")[0]
		}
		byName[strings.ToLower(name)] = i
	}
	row := rows.current
	for ci, cname := range rows.columnNames {
		fi, ok := byName[strings.ToLower(cname)]
		if !ok {
			return out, fmt.Errorf("jed: column %q has no matching field in %s", cname, rt.Name())
		}
		if err := assignValue(row[ci], rv.Field(fi).Addr().Interface()); err != nil {
			return out, fmt.Errorf("jed: column %q: %w", cname, err)
		}
	}
	return out, nil
}

// ───────────────────────────── Null[T] ─────────────────────────────

// Null is a value of type T that may be NULL, usable as a scan destination and as a query
// argument, like database/sql's sql.Null. T is any type Scan and the query methods accept.
type Null[T any] struct {
	// Val is the value. It is ignored when Valid is false; scanning NULL sets it to T's zero value.
	Val T
	// Valid reports whether the value is not NULL.
	Valid bool
}

// ScanJed implements [Scanner]. NULL sets Val to its zero value and Valid to false; any other value
// is converted into Val, and Valid is set to true.
func (n *Null[T]) ScanJed(v Value) error {
	if v.kind == ValNull {
		var z T
		n.Val, n.Valid = z, false
		return nil
	}
	n.Valid = true
	return assignValue(v, &n.Val)
}

// JedValue implements [Valuer]. It returns NULL when Valid is false, else Val converted as a query
// argument.
func (n Null[T]) JedValue() (Value, error) {
	if !n.Valid {
		return NullValue(), nil
	}
	return toValue(n.Val)
}

// ───────────────────────────── arg conversion (any → Value) ─────────────────────────────

func toValues(args []any) ([]Value, error) {
	if len(args) == 0 {
		return nil, nil
	}
	out := make([]Value, len(args))
	for i, a := range args {
		v, err := toValue(a)
		if err != nil {
			return nil, fmt.Errorf("jed: parameter $%d: %w", i+1, err)
		}
		out[i] = v
	}
	return out, nil
}

func toValue(a any) (Value, error) {
	switch v := a.(type) {
	case nil:
		return NullValue(), nil
	case Value:
		return v, nil
	case Valuer:
		return v.JedValue()
	case bool:
		return BoolValue(v), nil
	case int:
		return IntValue(int64(v)), nil
	case int8:
		return IntValue(int64(v)), nil
	case int16:
		return IntValue(int64(v)), nil
	case int32:
		return IntValue(int64(v)), nil
	case int64:
		return IntValue(v), nil
	case uint8:
		return IntValue(int64(v)), nil
	case uint16:
		return IntValue(int64(v)), nil
	case uint32:
		return IntValue(int64(v)), nil
	case uint, uint64:
		u := reflect.ValueOf(v).Uint()
		if u > math.MaxInt64 {
			return Value{}, fmt.Errorf("uint value %d overflows int64", u)
		}
		return IntValue(int64(u)), nil
	case float32:
		return Float32Value(v), nil
	case float64:
		return Float64Value(v), nil
	case string:
		// A Go string is arbitrary bytes; a jed text value is valid UTF-8 without NUL (types.md §11),
		// so the string is checked here as PostgreSQL checks a text-format parameter (22021).
		if _, err := decodeUTF8Text(v); err != nil {
			return Value{}, err
		}
		return TextValue(v), nil
	case []byte:
		return ByteaValue(v), nil
	case Decimal:
		return DecimalValue(v), nil
	case Interval:
		return IntervalValue(v), nil
	case time.Time:
		// time.Time binds as timestamptz here, carrying the time itself in ref so the §5 binder can
		// re-coerce it to the inferred temporal type (timestamp / date) by its calendar fields in its
		// own Location — the one Go↔jed temporal impedance point (api.md §11.2, hostTimeParam).
		tv := TimestamptzValue(v.UnixMicro())
		tv.ref = v
		return tv, nil
	default:
		// Any other slice binds as a one-dimensional array of its converted elements, for an array
		// parameter (`id = ANY($1)` — api.md §5); a nil slice is NULL.
		if rv := reflect.ValueOf(a); rv.Kind() == reflect.Slice {
			if rv.IsNil() {
				return NullValue(), nil
			}
			elems := make([]Value, rv.Len())
			for i := range elems {
				ev, err := toValue(rv.Index(i).Interface())
				if err != nil {
					return Value{}, fmt.Errorf("element %d: %w", i, err)
				}
				elems[i] = ev
			}
			return ArrayValue(elems), nil
		}
		return Value{}, fmt.Errorf("cannot use %T as a parameter", a)
	}
}

// ───────────────────────────── scan conversion (Value → *T) ─────────────────────────────

// Scan converts v into dest, a pointer, as [Rows.Scan] converts a column, including into
// *[ArrayVal] for an array, *[RangeVal] for a range, *[JSONNode] for jsonb and *[]Value for a
// composite's fields. The result is a copy the caller may modify. Scanning NULL into a plain
// pointer is an error; use *Null[T], *any, *Value or a [Scanner].
func (v Value) Scan(dest any) error { return scanOne(v, dest) }

func scanOne(v Value, dest any) error {
	if s, ok := dest.(Scanner); ok {
		return s.ScanJed(v)
	}
	return assignValue(v, dest)
}

// assignValue writes v into the pointer dest. The inline type switch covers the common types with
// no reflection; NULL into a plain scalar pointer errors (a Scanner / *any / *jed.Null[T] accepts
// NULL). It is also the shared one-dest converter used by RowTo / RowToStructByName / Null[T].
func assignValue(v Value, dest any) error {
	switch d := dest.(type) {
	case *any:
		*d = valueToAny(v)
		return nil
	case *Value:
		*d = v
		return nil
	}
	if s, ok := dest.(Scanner); ok {
		return s.ScanJed(v)
	}
	if v.kind == ValNull {
		return fmt.Errorf("NULL into %T (use *jed.Null[T], *any, or a Scanner)", dest)
	}
	switch d := dest.(type) {
	case *int64:
		n, err := asInt(v)
		if err != nil {
			return err
		}
		*d = n
	case *int:
		n, err := asInt(v)
		if err != nil {
			return err
		}
		if n < math.MinInt || n > math.MaxInt {
			return fmt.Errorf("value %d overflows int", n)
		}
		*d = int(n)
	case *int32:
		n, err := asInt(v)
		if err != nil {
			return err
		}
		if n < math.MinInt32 || n > math.MaxInt32 {
			return fmt.Errorf("value %d overflows int32", n)
		}
		*d = int32(n)
	case *int16:
		n, err := asInt(v)
		if err != nil {
			return err
		}
		if n < math.MinInt16 || n > math.MaxInt16 {
			return fmt.Errorf("value %d overflows int16", n)
		}
		*d = int16(n)
	case *bool:
		if v.kind != ValBool {
			return typeErr(v, "bool")
		}
		*d = v.boolVal()
	case *string:
		if v.kind != ValText {
			return typeErr(v, "string")
		}
		*d = v.str()
	case *[]byte:
		if v.kind != ValBytea && v.kind != ValUUID {
			return typeErr(v, "[]byte")
		}
		*d = []byte(v.str())
	case *float64:
		f, err := asFloat(v)
		if err != nil {
			return err
		}
		*d = f
	case *float32:
		f, err := asFloat(v)
		if err != nil {
			return err
		}
		*d = float32(f)
	case *Decimal:
		if v.kind != ValDecimal {
			return typeErr(v, "decimal")
		}
		*d = *v.decimal()
	case *Interval:
		if v.kind != ValInterval {
			return typeErr(v, "interval")
		}
		*d = v.interval()
	case *time.Time:
		t, err := asTime(v)
		if err != nil {
			return err
		}
		*d = t
	case *ArrayVal:
		if v.kind != ValArray {
			return typeErr(v, "ArrayVal")
		}
		a := v.arrayVal()
		*d = ArrayVal{Dims: slices.Clone(a.Dims), Lbounds: slices.Clone(a.Lbounds), Elements: slices.Clone(a.Elements)}
	case *RangeVal:
		if v.kind != ValRange {
			return typeErr(v, "RangeVal")
		}
		*d = *v.rangeVal()
	case *JSONNode:
		if v.kind != ValJSONB {
			return typeErr(v, "JSONNode")
		}
		*d = v.jsonb().clone()
	case *[]Value:
		if v.kind != ValComposite {
			return typeErr(v, "[]Value")
		}
		*d = slices.Clone(*v.composite())
	default:
		return fmt.Errorf("unsupported Scan destination %T", dest)
	}
	return nil
}

func valueToAny(v Value) any {
	switch v.kind {
	case ValNull:
		return nil
	case ValInt:
		return v.num
	case ValBool:
		return v.boolVal()
	case ValText:
		return v.str()
	case ValBytea:
		return []byte(v.str())
	case ValUUID:
		return renderUUID([]byte(v.str()))
	case ValDecimal:
		return *v.decimal()
	case ValInterval:
		return v.interval()
	case ValFloat32:
		return v.f32()
	case ValFloat64:
		return v.f64()
	case ValTimestamp, ValTimestamptz, ValDate:
		// An infinite value has no time.Time form (asTime): it stays the raw Value.
		if t, err := asTime(v); err == nil {
			return t
		}
		return v
	default:
		// composite / array / range / json: the raw Value for now (richer mapping is a follow-up).
		return v
	}
}

func asInt(v Value) (int64, error) {
	if v.kind != ValInt {
		return 0, typeErr(v, "int")
	}
	return v.num, nil
}

func asFloat(v Value) (float64, error) {
	switch v.kind {
	case ValFloat64:
		return v.f64(), nil
	case ValFloat32:
		return float64(v.f32()), nil
	case ValInt:
		return float64(v.num), nil
	default:
		return 0, typeErr(v, "float")
	}
}

// asTime converts a finite timestamp / timestamptz / date to a time.Time in UTC (api.md §11.2). An
// infinite value has no time.Time form — time.Time is finite — so it is an error, never a bogus
// instant (the ±infinity sentinels are the extreme i64/i32, which UnixMicro would render as a real
// date in the year ±292 277): Values() returns such a cell as its raw Value instead (valueToAny).
func asTime(v Value) (time.Time, error) {
	if isInfiniteTemporal(v) {
		return time.Time{}, fmt.Errorf("cannot scan an infinite %s into time.Time", kindName(v.kind))
	}
	switch v.kind {
	case ValTimestamp, ValTimestamptz:
		return time.UnixMicro(v.num).UTC(), nil
	case ValDate:
		return time.Unix(int64(int32(v.num))*86400, 0).UTC(), nil
	default:
		return time.Time{}, typeErr(v, "time.Time")
	}
}

// isInfiniteTemporal reports whether v is a ±infinity timestamp, timestamptz, or date.
func isInfiniteTemporal(v Value) bool {
	switch v.kind {
	case ValTimestamp, ValTimestamptz:
		return v.num == negInfinity || v.num == posInfinity
	case ValDate:
		return int32(v.num) == dateNegInfinity || int32(v.num) == datePosInfinity
	default:
		return false
	}
}

// hostTimeParam re-coerces a host time.Time bound as a timestamptz Value (toValue) to the parameter
// type the statement inferred (api.md §5/§11.2): the instant for timestamptz; for timestamp and date
// the time's calendar fields in its own Location — what a text literal of the time formatted without
// a zone would read as — so a date built at UTC midnight binds as that date whatever the session
// time zone. The result is a clean Value (no ref), and any other target keeps the timestamptz value,
// whose family mismatch the binder then reports as a literal's (42804). A nil ty is "no inferred
// scalar" (a container target): the value stays timestamptz.
func hostTimeParam(v Value, ty *scalarType) (Value, error) {
	t, ok := v.ref.(time.Time)
	if !ok {
		return v, nil
	}
	if ty != nil {
		switch {
		case ty.IsTimestamp():
			y, mo, d := t.Date()
			h, mi, sec := t.Clock()
			local := time.Date(y, mo, d, h, mi, sec, t.Nanosecond(), time.UTC)
			micros, err := checkedHostTimeMicros(local)
			return TimestampValue(micros), err
		case ty.IsDate():
			y, mo, d := t.Date()
			// Bound the year before calendar arithmetic, then check the finite
			// day range before narrowing to the stored i32 representation.
			if y < -6_000_000 || y > 6_000_000 {
				return Value{}, datetimeFieldOverflow("date out of range")
			}
			days := daysFromCivil(int64(y), int64(mo), int64(d))
			if days < dateMinFinite || days > dateMaxFinite {
				return Value{}, datetimeFieldOverflow("date out of range")
			}
			return DateValue(int32(days)), nil
		case ty.IsTimestamptz():
			micros, err := checkedHostTimeMicros(t)
			return TimestamptzValue(micros), err
		}
	}
	return TimestamptzValue(v.num), nil
}

// UnixMicro wraps outside the i64 microsecond range. Verify the conversion
// round-trips at the engine's precision and does not occupy an infinity sentinel.
func checkedHostTimeMicros(t time.Time) (int64, error) {
	micros := t.UnixMicro()
	if micros == negInfinity || micros == posInfinity || !time.UnixMicro(micros).Equal(t.Truncate(time.Microsecond)) {
		return 0, datetimeFieldOverflow("timestamp out of range")
	}
	return micros, nil
}

func typeErr(v Value, want string) error {
	return fmt.Errorf("cannot scan %s into %s", kindName(v.kind), want)
}

// kindName is a short label for a ValueKind, for Scan/typed-accessor error messages.
func kindName(k ValueKind) string {
	switch k {
	case ValNull:
		return "NULL"
	case ValInt:
		return "int"
	case ValBool:
		return "bool"
	case ValText:
		return "text"
	case ValDecimal:
		return "decimal"
	case ValBytea:
		return "bytea"
	case ValUUID:
		return "uuid"
	case ValTimestamp:
		return "timestamp"
	case ValTimestamptz:
		return "timestamptz"
	case ValDate:
		return "date"
	case ValInterval:
		return "interval"
	case ValFloat32:
		return "f32"
	case ValFloat64:
		return "f64"
	case ValComposite:
		return "composite"
	case ValArray:
		return "array"
	case ValRange:
		return "range"
	case ValJSON:
		return "json"
	case ValJSONB:
		return "jsonb"
	case ValJSONPath:
		return "jsonpath"
	default:
		return fmt.Sprintf("kind#%d", int(k))
	}
}

// ───────────────────────────── cancellation ─────────────────────────────

// contextDone captures the channel once per operation. A nil context is accepted
// like context.Background; no context or closure is retained by the cursor.
func contextDone(ctx context.Context) <-chan struct{} {
	if ctx == nil {
		return nil
	}
	return ctx.Done()
}

// doneErr is the immediate cancellation-only check at API, cursor, and batch
// boundaries. A nil channel disables cancellation without blocking.
func doneErr(done <-chan struct{}) error {
	if done == nil {
		return nil
	}
	select {
	case <-done:
		return newError(QueryCanceled, "canceling statement due to user request")
	default:
		return nil
	}
}

// contextError enriches only observed cancellation errors. Never infer failure
// from a context that was canceled after a successful operation or commit.
func contextError(ctx context.Context, err error) error {
	if ctx == nil || err == nil {
		return err
	}
	return cancellationCause(err, ctx.Err)
}

func cancellationCause(err error, cause func() error) error {
	if cause == nil {
		return err
	}
	if ee, ok := err.(*EngineError); ok && ee.State == QueryCanceled && ee.cause == nil {
		// Errors can be retained by other consumers; attach host metadata to a copy.
		copy := *ee
		copy.cause = cause()
		return &copy
	}
	return err
}

// armCancel temporarily installs an operation-owned channel. Meters and snapshot
// engines copy it, so restoring the session cannot disarm an existing cursor or
// let its cancellation affect a later call. No goroutine or closure is needed.
func (e *engine) armCancel(done <-chan struct{}) <-chan struct{} {
	previous := e.session.cancel
	e.session.cancel = done
	return previous
}

func (e *engine) restoreCancel(previous <-chan struct{}) {
	e.session.cancel = previous
}
