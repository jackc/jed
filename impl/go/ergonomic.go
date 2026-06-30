package jed

// Ergonomic host bindings (spec/design/api.md §11) — a pgx-style layer over the typed Value
// surface. Three layers, one []Value currency underneath:
//
//   - Query / Exec / QueryRow take plain Go args (...any) and a context.Context, and return a
//     cursor whose Scan converts into Go-native destinations. This is the default surface.
//   - Typed accessors (Rows.Int/Text/...) skip the Scan type switch for hot loops.
//   - Rows.Value / the raw []Value path (api.go) stay for full fidelity.
//
// Cancellation is wired through the cost meter: each Query/Exec/QueryRow arms a poll on the engine
// that runs the statement (armCancel), and the meter's Guard() checkpoint consults it, so a flipped
// context.Context aborts a long-running statement with 57014 at the next metering point — not only
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
	"strings"
	"sync/atomic"
	"time"
)

// ctxIface is context.Context under a local name so api.go can hold a ctx field without importing
// context (it carries no other context use).
type ctxIface = context.Context

// ErrNoRows is returned by Row.Scan when the query produced no rows (database/sql's sentinel).
var ErrNoRows = errors.New("jed: no rows in result set")

// Valuer lets a host type convert itself to a query parameter (the arg-conversion escape hatch).
type Valuer interface{ JedValue() (Value, error) }

// Scanner lets a host type receive a Value during Scan (the scan-target escape hatch).
type Scanner interface{ ScanJed(v Value) error }

// Queryer is satisfied by *Database (and, once the raw []Value methods are renamed to *SQL,
// *Transaction — api.md §11) so helpers can take either a handle or a transaction.
type Queryer interface {
	Query(ctx context.Context, sql string, args ...any) (*Rows, error)
	QueryRow(ctx context.Context, sql string, args ...any) *Row
	Exec(ctx context.Context, sql string, args ...any) (Result, error)
}

var (
	_ Queryer = (*Database)(nil)
	_ Queryer = (*Transaction)(nil)
	// *PreparedStatement deliberately does NOT satisfy Queryer: its SQL is fixed at Prepare, so
	// its ergonomic methods take (ctx, args...) with no sql parameter.
)

// ───────────────────────────── Query / Exec / QueryRow ─────────────────────────────
//
// The same three ergonomic methods sit on *Database (autocommit), *Transaction (inside an explicit
// block), and *PreparedStatement (fixed SQL). Each converts native args and arms cancellation on
// the engine that runs the statement, so the conversion + cancellation logic lives once, in
// ergoQuery/ergoExec; the per-type methods are one-liners that supply the right engine + raw
// []Value primitive. armCancel threads ctx into the statement's cost meter (api.md §11.4), so a
// flipped context aborts a long-running statement at the executor's Guard() checkpoint — not only
// at the cursor boundary (ctxErr, the cheap pre-exec / cursor poll).

func ergoQuery(ctx context.Context, eng *engine, args []any, raw func([]Value) (*Rows, error)) (*Rows, error) {
	params, err := toValues(args)
	if err != nil {
		return nil, err
	}
	if err := ctxErr(ctx); err != nil {
		return nil, err
	}
	defer eng.armCancel(ctx)()
	rows, err := raw(params)
	if err != nil {
		return nil, err
	}
	rows.ctx = ctx
	return rows, nil
}

func ergoExec(ctx context.Context, eng *engine, args []any, raw func([]Value) (Outcome, error)) (Result, error) {
	params, err := toValues(args)
	if err != nil {
		return Result{}, err
	}
	if err := ctxErr(ctx); err != nil {
		return Result{}, err
	}
	defer eng.armCancel(ctx)()
	out, err := raw(params)
	if err != nil {
		return Result{}, err
	}
	return Result{rowsAffected: out.RowsAffected, hasAffected: out.HasRowsAffected, cost: out.Cost}, nil
}

// Query runs a query on the autocommit handle, binding native args. It mints the fresh autocommit
// session explicitly (rather than via QueryValues) so cancellation arms on the engine that runs
// the statement.
func (db *Database) Query(ctx context.Context, sql string, args ...any) (*Rows, error) {
	s := db.Session(SessionOptions{})
	defer s.Close()
	return ergoQuery(ctx, s.engine, args, func(p []Value) (*Rows, error) { return s.Query(sql, p) })
}

// Exec runs a non-query statement on the autocommit handle and returns its command tag.
func (db *Database) Exec(ctx context.Context, sql string, args ...any) (Result, error) {
	s := db.Session(SessionOptions{})
	defer s.Close()
	return ergoExec(ctx, s.engine, args, func(p []Value) (Outcome, error) { return s.Execute(sql, p) })
}

// QueryRow runs a query and returns a one-row handle; a setup error defers to Row.Scan.
func (db *Database) QueryRow(ctx context.Context, sql string, args ...any) *Row {
	rows, err := db.Query(ctx, sql, args...)
	return &Row{rows: rows, err: err}
}

// Query / Exec / QueryRow inside an explicit transaction — identical shape to *Database, so a
// data-access function written against Queryer runs on either a handle or a transaction. The
// statement runs on tx.db, so cancellation arms there.
func (tx *Transaction) Query(ctx context.Context, sql string, args ...any) (*Rows, error) {
	return ergoQuery(ctx, tx.db, args, func(p []Value) (*Rows, error) { return tx.QueryValues(sql, p) })
}

func (tx *Transaction) Exec(ctx context.Context, sql string, args ...any) (Result, error) {
	return ergoExec(ctx, tx.db, args, func(p []Value) (Outcome, error) { return tx.Execute(sql, p) })
}

func (tx *Transaction) QueryRow(ctx context.Context, sql string, args ...any) *Row {
	rows, err := tx.Query(ctx, sql, args...)
	return &Row{rows: rows, err: err}
}

// Query / Exec / QueryRow on a prepared statement — SQL is fixed at Prepare, so there is no sql
// argument. The raw QueryValues/Execute methods match the helper closures directly; the statement
// runs on execEngine (the bound session's engine, else the statement's own).
func (s *PreparedStatement) Query(ctx context.Context, args ...any) (*Rows, error) {
	return ergoQuery(ctx, s.execEngine(), args, s.QueryValues)
}

func (s *PreparedStatement) Exec(ctx context.Context, args ...any) (Result, error) {
	return ergoExec(ctx, s.execEngine(), args, s.Execute)
}

func (s *PreparedStatement) QueryRow(ctx context.Context, args ...any) *Row {
	rows, err := s.Query(ctx, args...)
	return &Row{rows: rows, err: err}
}

// ───────────────────────────── Result (command tag) ─────────────────────────────

// Result is the command tag of a non-query statement.
type Result struct {
	rowsAffected int64
	hasAffected  bool
	cost         int64
}

// RowsAffected reports how many rows a DML statement touched; ok is false for DDL / transaction
// control (which carry no count, mirroring PostgreSQL — api.md §4).
func (r Result) RowsAffected() (n int64, ok bool) { return r.rowsAffected, r.hasAffected }

// Cost is the deterministic execution cost accrued (CLAUDE.md §13).
func (r Result) Cost() int64 { return r.cost }

// ───────────────────────────── Row (single-row) ─────────────────────────────

// Row is a one-row result handle from QueryRow.
type Row struct {
	rows *Rows
	err  error
}

// Scan reads the single row into dest, then closes the cursor. ErrNoRows if the query was empty;
// extra rows are ignored (database/sql / pgx semantics).
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

// Scan converts the current row's columns into the pointer destinations, one per column. It uses
// an inline type switch with explicit cases for the common types and never reflects on this path,
// and it does not let dest escape — so it is allocation-free (api.md §11). NULL into a plain
// scalar pointer is an error; use *jed.Null[T], *any, or a Scanner to accept NULL.
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

// Values returns the current row as natural Go values (pgx's Values) — for callers that do not
// know the schema statically. Scalars map to Go primitives; rich types map to their jed value
// type (a richer container mapping is an api.md §11 follow-up).
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

// Err returns the terminal error reached during iteration (a canceled ctx today; mid-stream
// faults once the cursor streams). Check it after the loop.
func (r *Rows) Err() error { return r.err }

// Close releases the cursor (its pinned read snapshot once streaming — streaming.md §5). A no-op
// over today's materialized result, but the contract the streaming cursor needs — and what
// All()/Collect close automatically on loop exit.
func (r *Rows) Close() error {
	r.cursor.close()
	return nil
}

// ColumnTypes is the canonical jed type name of each output column (parallel to ColumnNames).
func (r *Rows) ColumnTypes() []string { return r.columnTypes }

// ───────────────────────────── Rows: typed fast path ─────────────────────────────

// Int returns column col as an int64 (the hot-loop accessor — skips the Scan type switch).
func (r *Rows) Int(col int) (int64, error) {
	v, err := r.col(col)
	if err != nil {
		return 0, err
	}
	if v.Kind != ValInt {
		return 0, typeErr(v, "int")
	}
	return v.Int, nil
}

// Text returns column col as a string.
func (r *Rows) Text(col int) (string, error) {
	v, err := r.col(col)
	if err != nil {
		return "", err
	}
	if v.Kind != ValText {
		return "", typeErr(v, "text")
	}
	return v.Str, nil
}

// Bool returns column col as a bool.
func (r *Rows) Bool(col int) (bool, error) {
	v, err := r.col(col)
	if err != nil {
		return false, err
	}
	if v.Kind != ValBool {
		return false, typeErr(v, "bool")
	}
	return v.Bool, nil
}

// Float returns column col as a float64 (either float width widens).
func (r *Rows) Float(col int) (float64, error) {
	v, err := r.col(col)
	if err != nil {
		return 0, err
	}
	return asFloat(v)
}

// Bytes returns column col as a []byte (bytea or uuid).
func (r *Rows) Bytes(col int) ([]byte, error) {
	v, err := r.col(col)
	if err != nil {
		return nil, err
	}
	if v.Kind != ValBytea && v.Kind != ValUuid {
		return nil, typeErr(v, "bytea")
	}
	return []byte(v.Str), nil
}

// IsNull reports whether column col of the current row is SQL NULL.
func (r *Rows) IsNull(col int) bool {
	v, err := r.col(col)
	return err == nil && v.Kind == ValNull
}

// Value returns column col of the current row as the raw engine Value (full fidelity).
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

// All returns a single-use iterator over the remaining rows; the yielded *Rows is positioned at
// the current row (call Scan/typed accessors on it). The iterator Closes the cursor on loop exit
// — break, return, panic, or exhaustion — so no `defer rows.Close()` is needed. Check rows.Err()
// after the loop for a terminal error (e.g. a canceled context).
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

// Collect returns an iterator that maps each row through fn and yields (value, error). A terminal
// stream error (a canceled ctx, a future mid-stream fault) is delivered as a final (zero, err)
// pair. The cursor is Closed on loop exit. Pair with RowToStructByName for struct mapping:
//
//	for u, err := range jed.Collect(rows, jed.RowToStructByName[User]) { ... }
func Collect[T any](rows *Rows, fn func(*Rows) (T, error)) iter.Seq2[T, error] {
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

// RowTo scans a single-column row into a T (for `SELECT count(*)`-shaped queries).
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

// RowToStructByName maps the current row into a struct T by matching column names against `db:"…"`
// tags (falling back to the field name, case-insensitively). This is the convenience path: it
// reflects once per row, which is fine off the hot loop — use Scan / typed accessors when it isn't.
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
		name := f.Name
		if tag := f.Tag.Get("db"); tag != "" && tag != "-" {
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

// Null[T] is a nullable scan target / parameter for any supported scalar T (the generic analog of
// sql.NullInt64 etc.). It implements Scanner and Valuer, so it slots into Scan and the arg path
// without a per-type case.
type Null[T any] struct {
	Val   T
	Valid bool
}

// ScanJed receives a Value: NULL clears Valid, anything else scans into Val.
func (n *Null[T]) ScanJed(v Value) error {
	if v.Kind == ValNull {
		var z T
		n.Val, n.Valid = z, false
		return nil
	}
	n.Valid = true
	return assignValue(v, &n.Val)
}

// JedValue produces the parameter Value: NULL when !Valid, else Val converted.
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
		return TextValue(v), nil
	case []byte:
		return ByteaValue(v), nil
	case Decimal:
		return DecimalValue(v), nil
	case Interval:
		return IntervalValue(v), nil
	case time.Time:
		// time.Time binds as timestamptz here; the binder re-coerces to the inferred temporal
		// column type (timestamp / date) — the one Go↔jed temporal impedance point (api.md §11).
		return TimestamptzValue(v.UnixMicro()), nil
	default:
		return Value{}, fmt.Errorf("cannot use %T as a parameter", a)
	}
}

// ───────────────────────────── scan conversion (Value → *T) ─────────────────────────────

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
	if v.Kind == ValNull {
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
		if v.Kind != ValBool {
			return typeErr(v, "bool")
		}
		*d = v.Bool
	case *string:
		if v.Kind != ValText {
			return typeErr(v, "string")
		}
		*d = v.Str
	case *[]byte:
		if v.Kind != ValBytea && v.Kind != ValUuid {
			return typeErr(v, "[]byte")
		}
		*d = []byte(v.Str)
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
		if v.Kind != ValDecimal {
			return typeErr(v, "decimal")
		}
		*d = *v.Dec
	case *Interval:
		if v.Kind != ValInterval {
			return typeErr(v, "interval")
		}
		*d = v.Iv
	case *time.Time:
		t, err := asTime(v)
		if err != nil {
			return err
		}
		*d = t
	default:
		return fmt.Errorf("unsupported Scan destination %T", dest)
	}
	return nil
}

func valueToAny(v Value) any {
	switch v.Kind {
	case ValNull:
		return nil
	case ValInt:
		return v.Int
	case ValBool:
		return v.Bool
	case ValText:
		return v.Str
	case ValBytea:
		return []byte(v.Str)
	case ValUuid:
		return renderUUID([]byte(v.Str))
	case ValDecimal:
		return *v.Dec
	case ValInterval:
		return v.Iv
	case ValFloat32:
		return v.F32()
	case ValFloat64:
		return v.F64()
	case ValTimestamp, ValTimestamptz:
		return time.UnixMicro(v.Int).UTC()
	case ValDate:
		return time.Unix(int64(int32(v.Int))*86400, 0).UTC()
	default:
		// composite / array / range / json: the raw Value for now (richer mapping is a follow-up).
		return v
	}
}

func asInt(v Value) (int64, error) {
	if v.Kind != ValInt {
		return 0, typeErr(v, "int")
	}
	return v.Int, nil
}

func asFloat(v Value) (float64, error) {
	switch v.Kind {
	case ValFloat64:
		return v.F64(), nil
	case ValFloat32:
		return float64(v.F32()), nil
	case ValInt:
		return float64(v.Int), nil
	default:
		return 0, typeErr(v, "float")
	}
}

func asTime(v Value) (time.Time, error) {
	switch v.Kind {
	case ValTimestamp, ValTimestamptz:
		return time.UnixMicro(v.Int).UTC(), nil
	case ValDate:
		return time.Unix(int64(int32(v.Int))*86400, 0).UTC(), nil
	default:
		return time.Time{}, typeErr(v, "time.Time")
	}
}

func typeErr(v Value, want string) error {
	return fmt.Errorf("cannot scan %s into %s", kindName(v.Kind), want)
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
	case ValUuid:
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
	case ValJson:
		return "json"
	case ValJsonb:
		return "jsonb"
	case ValJsonPath:
		return "jsonpath"
	default:
		return fmt.Sprintf("kind#%d", int(k))
	}
}

// ───────────────────────────── cancellation ─────────────────────────────

// ctxErr reports a non-blocking cancellation check on ctx — the cheap poll at the API entry and
// cursor boundary (the in-statement poll is the meter's Guard, armed by armCancel). A nil or
// already-canceled ctx is handled here; a background ctx (nil Done channel) costs nothing.
func ctxErr(ctx ctxIface) error {
	if ctx == nil {
		return nil
	}
	select {
	case <-ctx.Done():
		return newError(QueryCanceled, "canceling statement due to user request")
	default:
		return nil
	}
}

// armCancel installs a cancellation poll on this engine's session for the duration of one
// statement, returning a restore func that clears it (call via `defer eng.armCancel(ctx)()`). The
// meter minted during the statement copies the poll (sessionState.newMeter) and consults it at each
// Guard() checkpoint, so a flipped ctx aborts a long-running statement with 57014 — not only at the
// cursor boundary (api.md §11.4). A nil or non-cancelable ctx (background context: nil Done channel)
// installs nothing, so the hot path stays untouched in the overwhelmingly common case. For a live
// ctx, one watcher goroutine flips an atomic on Done, keeping the per-checkpoint poll to a single
// atomic load (the watcher is torn down by the returned restore — no leak). The engine that runs a
// statement is single-goroutine for its duration, so session.cancel is set/read/cleared without a
// data race; only the atomic crosses goroutines.
func (e *engine) armCancel(ctx context.Context) func() {
	if ctx == nil || ctx.Done() == nil {
		return func() {}
	}
	var canceled atomic.Bool
	done := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			canceled.Store(true)
		case <-done:
		}
	}()
	e.session.cancel = func() bool { return canceled.Load() }
	return func() {
		close(done)
		e.session.cancel = nil
	}
}

// execEngine is the engine a prepared statement's run dispatches on: the bound session's engine
// when prepared on a Session, else the statement's own engine.
func (s *PreparedStatement) execEngine() *engine {
	if s.sess != nil {
		return s.sess.engine
	}
	return s.db
}
