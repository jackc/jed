package abide

import (
	"fmt"
	"math"
	"sort"
	"strings"
)

// Statement executor (CLAUDE.md §10).
//
// SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
// feature-by-feature (Phases B–E).

// OutcomeKind distinguishes a bare statement result from a query result set.
type OutcomeKind int

const (
	// OutcomeStatement is a statement producing no result set (CREATE, INSERT).
	OutcomeStatement OutcomeKind = iota
	// OutcomeQuery is a query result set.
	OutcomeQuery
)

// Outcome is the result of executing one statement. Cost is the deterministic execution
// cost accrued while running it (CLAUDE.md §13) — a DML statement accrues its scan +
// filter cost even though it returns no rows.
type Outcome struct {
	Kind        OutcomeKind
	ColumnCount int
	Rows        [][]Value
	Cost        int64
}

// Database is the whole database: catalog + per-table in-memory stores. Single
// committed state (CLAUDE.md §3); the staging-buffer commit model lands with
// persistence.
type Database struct {
	tables map[string]*Table
	stores map[string]*TableStore
}

// NewDatabase builds an empty database.
func NewDatabase() *Database {
	return &Database{
		tables: make(map[string]*Table),
		stores: make(map[string]*TableStore),
	}
}

// Table looks up a table definition by name (case-insensitive).
func (db *Database) Table(name string) (*Table, bool) {
	t, ok := db.tables[strings.ToLower(name)]
	return t, ok
}

// putTable registers a new table and its empty store.
func (db *Database) putTable(t *Table) {
	key := strings.ToLower(t.Name)
	db.stores[key] = NewTableStore()
	db.tables[key] = t
}

// ExecuteStmt executes one parsed statement.
func (db *Database) ExecuteStmt(stmt Statement) (Outcome, error) {
	switch {
	case stmt.CreateTable != nil:
		return db.executeCreateTable(stmt.CreateTable)
	case stmt.Insert != nil:
		return db.executeInsert(stmt.Insert)
	case stmt.Select != nil:
		return db.executeSelect(stmt.Select)
	case stmt.Update != nil:
		return db.executeUpdate(stmt.Update)
	case stmt.Delete != nil:
		return db.executeDelete(stmt.Delete)
	default:
		return Outcome{}, NewError(SyntaxError, "empty statement")
	}
}

// executeCreateTable analyzes and runs a CREATE TABLE: resolve each column's type
// name, enforce a single primary key (which is implicitly NOT NULL), reject
// duplicate table and column names, then register the table.
func (db *Database) executeCreateTable(ct *CreateTable) (Outcome, error) {
	if _, ok := db.Table(ct.Name); ok {
		return Outcome{}, NewError(DuplicateTable, "table already exists: "+ct.Name)
	}

	columns := make([]Column, 0, len(ct.Columns))
	pkSeen := false
	for _, def := range ct.Columns {
		for _, c := range columns {
			if strings.EqualFold(c.Name, def.Name) {
				return Outcome{}, NewError(DuplicateColumn, "duplicate column name: "+def.Name)
			}
		}
		ty, err := resolveStorableType(def.TypeName)
		if err != nil {
			return Outcome{}, err
		}
		if def.PrimaryKey {
			if pkSeen {
				return Outcome{}, NewError(InvalidTableDefinition,
					"a table may have at most one primary key")
			}
			pkSeen = true
		}
		columns = append(columns, Column{
			Name:       def.Name,
			Type:       ty,
			PrimaryKey: def.PrimaryKey,
			NotNull:    def.PrimaryKey, // PRIMARY KEY ⇒ NOT NULL
		})
	}

	db.putTable(&Table{Name: ct.Name, Columns: columns})
	// DDL touches no rows and evaluates no expressions: zero cost.
	return Outcome{Kind: OutcomeStatement, Cost: 0}, nil
}

// executeInsert analyzes and runs an INSERT: map the literal values positionally to
// columns, type-check each (NULL into NOT NULL traps 23502; an integer outside the
// column type's range traps 22003 — CLAUDE.md §8), then store the row keyed by its
// encoded primary key (duplicate key traps 23505).
func (db *Database) executeInsert(ins *Insert) (Outcome, error) {
	table, ok := db.Table(ins.Table)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+ins.Table)
	}
	if len(ins.Values) != len(table.Columns) {
		return Outcome{}, NewError(SyntaxError, fmt.Sprintf(
			"INSERT has %d values but table %s has %d columns",
			len(ins.Values), table.Name, len(table.Columns),
		))
	}

	row := make(Row, len(table.Columns))
	for i, col := range table.Columns {
		lit := ins.Values[i]
		switch lit.Kind {
		case LiteralNull:
			if col.NotNull {
				return Outcome{}, NewError(NotNullViolation,
					"null value in column "+col.Name+" violates not-null constraint")
			}
			row[i] = NullValue()
		case LiteralInt:
			if !col.Type.InRange(lit.Int) {
				return Outcome{}, NewError(NumericValueOutOfRange,
					"value out of range for type "+col.Type.CanonicalName())
			}
			row[i] = IntValue(lit.Int)
		case LiteralBool:
			// boolean is expression-only: there are no boolean columns, so a boolean
			// literal can only target an integer column — a type error (42804).
			return Outcome{}, NewError(DatatypeMismatch,
				"cannot store a boolean value in integer column "+col.Name)
		}
	}

	// The storage key is the encoded primary key, or — for a table without one — a
	// monotonic synthetic rowid: never reused, so DELETE then INSERT cannot collide
	// with a freed key (spec/fileformat/format.md).
	store := db.stores[strings.ToLower(ins.Table)]
	var key []byte
	if pk := table.PrimaryKeyIndex(); pk >= 0 {
		key = EncodeInt(table.Columns[pk].Type, row[pk].Int)
	} else {
		key = EncodeInt(Int64, store.AllocRowid())
	}

	if !store.Insert(key, row) {
		return Outcome{}, NewError(UniqueViolation,
			"duplicate key value violates primary key uniqueness")
	}
	// A single-row INSERT of literal values reads no rows and evaluates no expression
	// tree: zero cost (DEFAULT expressions, when added, will accrue here).
	return Outcome{Kind: OutcomeStatement, Cost: 0}, nil
}

// executeDelete analyzes and runs a DELETE: resolve the table and optional predicate,
// collect the keys of matching rows (only a TRUE predicate matches — Kleene), then
// remove them. No WHERE deletes every row. Keys are collected before mutating so the
// map is not modified while iterating.
func (db *Database) executeDelete(del *Delete) (Outcome, error) {
	table, ok := db.Table(del.Table)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+del.Table)
	}
	var filter *rExpr
	if del.Filter != nil {
		f, err := resolveBooleanFilter(table, del.Filter)
		if err != nil {
			return Outcome{}, err
		}
		filter = f
	}

	// Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13;
	// spec/design/cost.md §3). Keys are collected before mutating (so the map is not
	// modified mid-scan).
	meter := NewMeter()
	store := db.stores[strings.ToLower(del.Table)]
	var keys [][]byte
	for _, e := range store.EntriesInKeyOrder() {
		meter.Charge(Costs.StorageRowRead)
		matched := true
		if filter != nil {
			v, err := filter.eval(e.Row, meter)
			if err != nil {
				return Outcome{}, err
			}
			matched = v.IsTrue()
		}
		if matched {
			keys = append(keys, e.Key)
		}
	}
	for _, k := range keys {
		store.Remove(k)
	}
	return Outcome{Kind: OutcomeStatement, Cost: meter.Accrued}, nil
}

// executeUpdate analyzes and runs an UPDATE. Two-phase / all-or-nothing: phase 1
// builds and type-checks every matching row's new values (assignments evaluate
// against the old row, so `SET a = b, b = a` swaps); a 22003/23502 aborts with no
// writes. Phase 2 applies. Assigning a PRIMARY KEY column traps 0A000 (the storage
// key must not change this slice); a duplicate target column traps 42701. No WHERE
// updates every row.
func (db *Database) executeUpdate(upd *Update) (Outcome, error) {
	table, ok := db.Table(upd.Table)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+upd.Table)
	}

	// Resolve assignments up front (fail fast, deterministic).
	pkIdx := table.PrimaryKeyIndex()
	plans := make([]assignPlan, 0, len(upd.Assignments))
	for _, a := range upd.Assignments {
		idx := table.ColumnIndex(a.Column)
		if idx < 0 {
			return Outcome{}, NewError(UndefinedColumn, "column does not exist: "+a.Column)
		}
		if idx == pkIdx {
			return Outcome{}, NewError(FeatureNotSupported,
				"updating a primary key column is not supported")
		}
		for _, p := range plans {
			if p.idx == idx {
				return Outcome{}, NewError(DuplicateColumn,
					"column "+a.Column+" assigned more than once")
			}
		}
		col := table.Columns[idx]
		// The RHS is a general expression evaluated against the *old* row; a literal
		// operand adapts to the target column's type. The result must be an integer (or
		// NULL) — assigning a boolean to an integer column is a 42804.
		src, ty, err := resolve(table, a.Value, &col.Type)
		if err != nil {
			return Outcome{}, err
		}
		if err := requireAssignableInt(ty, a.Column); err != nil {
			return Outcome{}, err
		}
		plans = append(plans, assignPlan{
			idx: idx, name: col.Name, target: col.Type, notNull: col.NotNull, source: src,
		})
	}

	var filter *rExpr
	if upd.Filter != nil {
		f, err := resolveBooleanFilter(table, upd.Filter)
		if err != nil {
			return Outcome{}, err
		}
		filter = f
	}

	// Phase 1: build + validate every matching row's new values; no writes yet. Each
	// scanned row, the filter, and each assignment RHS accrue cost (the phase-2 writes
	// do not — they evaluate nothing; spec/design/cost.md §3).
	meter := NewMeter()
	store := db.stores[strings.ToLower(upd.Table)]
	type pending struct {
		key []byte
		row Row
	}
	var updates []pending
	for _, e := range store.EntriesInKeyOrder() {
		meter.Charge(Costs.StorageRowRead)
		if filter != nil {
			v, err := filter.eval(e.Row, meter)
			if err != nil {
				return Outcome{}, err
			}
			if !v.IsTrue() {
				continue
			}
		}
		newRow := make(Row, len(e.Row))
		copy(newRow, e.Row)
		for _, p := range plans {
			raw, err := p.source.eval(e.Row, meter)
			if err != nil {
				return Outcome{}, err
			}
			checked, err := p.check(raw)
			if err != nil {
				return Outcome{}, err
			}
			newRow[p.idx] = checked
		}
		updates = append(updates, pending{key: e.Key, row: newRow})
	}

	// Phase 2: apply (keys unchanged — a PK column can't be assigned).
	for _, u := range updates {
		store.Replace(u.key, u.row)
	}
	return Outcome{Kind: OutcomeStatement, Cost: meter.Accrued}, nil
}

// RowsInKeyOrder returns a table's rows in primary-key (encoded byte) order, or nil
// if the table does not exist. Used by SELECT and by tests.
func (db *Database) RowsInKeyOrder(name string) []Row {
	store, ok := db.stores[strings.ToLower(name)]
	if !ok {
		return nil
	}
	return store.IterInKeyOrder()
}

// executeSelect analyzes and runs a SELECT: resolve projected columns and the
// WHERE/ORDER BY columns against the catalog, scan the table in primary-key order,
// filter by the predicate (three-valued — only TRUE keeps a row), optionally re-sort
// by ORDER BY, then project. Rows are produced deterministically (CLAUDE.md §10).
func (db *Database) executeSelect(sel *Select) (Outcome, error) {
	table, ok := db.Table(sel.From)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+sel.From)
	}

	projections, err := resolveProjections(table, sel.Items)
	if err != nil {
		return Outcome{}, err
	}

	var filter *rExpr
	if sel.Filter != nil {
		filter, err = resolveBooleanFilter(table, sel.Filter)
		if err != nil {
			return Outcome{}, err
		}
	}

	orderIdx, orderDesc := -1, false
	if sel.OrderBy != nil {
		orderIdx = table.ColumnIndex(sel.OrderBy.Column)
		if orderIdx < 0 {
			return Outcome{}, NewError(UndefinedColumn, "column does not exist: "+sel.OrderBy.Column)
		}
		orderDesc = sel.OrderBy.Descending
	}

	// Scan in primary-key order, then filter. A WHERE arithmetic can trap
	// (22003/22012), so the error is propagated rather than swallowed in a predicate.
	// Each scanned row and the filter evaluation accrue cost; the row-produced charge is
	// below, at projection (CLAUDE.md §13; spec/design/cost.md §3).
	meter := NewMeter()
	var rows []Row
	for _, row := range db.RowsInKeyOrder(sel.From) {
		meter.Charge(Costs.StorageRowRead)
		keep := true
		if filter != nil {
			v, err := filter.eval(row, meter)
			if err != nil {
				return Outcome{}, err
			}
			keep = v.IsTrue()
		}
		if keep {
			rows = append(rows, row)
		}
	}

	// ORDER BY: stable sort by the key column's value, NULLs first ascending
	// (spec/design/encoding.md §4); descending reverses, NULLs last.
	if orderIdx >= 0 {
		sort.SliceStable(rows, func(a, b int) bool {
			c := nullFirstCmp(rows[a][orderIdx], rows[b][orderIdx])
			if orderDesc {
				return c > 0
			}
			return c < 0
		})
	}

	// Project each surviving row. Producing a row, and each projection-list evaluation,
	// accrue cost. (ORDER BY's sort comparisons are not metered — spec/design/cost.md §3.)
	out := make([][]Value, 0, len(rows))
	for _, row := range rows {
		meter.Charge(Costs.RowProduced)
		projected := make([]Value, len(projections))
		for i, p := range projections {
			v, err := p.eval(row, meter)
			if err != nil {
				return Outcome{}, err
			}
			projected[i] = v
		}
		out = append(out, projected)
	}

	return Outcome{Kind: OutcomeQuery, ColumnCount: len(projections), Rows: out, Cost: meter.Accrued}, nil
}

// ============================================================================
// Resolved expression layer (mirrors impl/rust executor.rs).
//
// Parse → Expr (names) → resolve → rExpr (column indices, known result types, folded
// constants) → eval per row → Value. The resolver is where all type-checking and the
// literal range-check live; the evaluator is a pure tree-walk.
// ============================================================================

// rtKind tags the static type of a resolved expression.
type rtKind int

const (
	rtNull rtKind = iota // an untyped NULL literal
	rtInt                // integer; intTy carries the ScalarType
	rtBool
)

type resolvedType struct {
	kind  rtKind
	intTy ScalarType // valid when kind == rtInt
}

func intType(t resolvedType) (ScalarType, bool) {
	if t.kind == rtInt {
		return t.intTy, true
	}
	return 0, false
}

// ctxOf returns the integer type of t as a context for a sibling literal, or nil if t
// is not integer (so a sibling literal defaults to int64).
func ctxOf(t resolvedType) *ScalarType {
	if t.kind == rtInt {
		ty := t.intTy
		return &ty
	}
	return nil
}

// rExprKind tags a resolved expression node.
type rExprKind int

const (
	reColumn rExprKind = iota
	reConstInt
	reConstBool
	reConstNull
	reCast
	reNeg
	reNot
	reArith
	reCompare
	reAnd
	reOr
	reIsNull
	reDistinct
)

// rExpr is a resolved expression over fixed column indices, ready to evaluate against a
// row. Arithmetic/neg nodes carry their (promotion-tower) result type in `result` so the
// computed value can be range-checked against it.
type rExpr struct {
	kind    rExprKind
	index   int        // reColumn
	cInt    int64      // reConstInt
	cBool   bool       // reConstBool
	op      BinaryOp   // reArith, reCompare
	result  ScalarType // reCast target; reNeg / reArith result type
	lhs     *rExpr     // reArith, reCompare, reAnd, reOr, reDistinct
	rhs     *rExpr     // reArith, reCompare, reAnd, reOr, reDistinct
	operand *rExpr     // reCast, reNeg, reNot, reIsNull
	negated bool       // reIsNull, reDistinct
}

// resolveProjections resolves SELECT items into evaluable projections (any result type
// is allowed in the select list, including boolean — SELECT a = b).
func resolveProjections(table *Table, items SelectItems) ([]*rExpr, error) {
	if items.All {
		ps := make([]*rExpr, len(table.Columns))
		for i := range table.Columns {
			ps[i] = &rExpr{kind: reColumn, index: i}
		}
		return ps, nil
	}
	ps := make([]*rExpr, 0, len(items.Items))
	for _, e := range items.Items {
		node, _, err := resolve(table, e, nil)
		if err != nil {
			return nil, err
		}
		ps = append(ps, node)
	}
	return ps, nil
}

// resolveBooleanFilter resolves a WHERE expression; it must resolve to boolean (or an
// untyped NULL, which is always unknown → no rows). An integer-valued WHERE is a 42804.
func resolveBooleanFilter(table *Table, e *Expr) (*rExpr, error) {
	node, ty, err := resolve(table, *e, nil)
	if err != nil {
		return nil, err
	}
	if ty.kind == rtInt {
		return nil, typeError("argument of WHERE must be boolean")
	}
	return node, nil
}

// resolve resolves one Expr into an rExpr plus its static type. ctx (non-nil) is the
// type an untyped integer literal should adapt to (spec/design/types.md §6); nil
// defaults a bare literal to int64.
func resolve(table *Table, e Expr, ctx *ScalarType) (*rExpr, resolvedType, error) {
	switch e.Kind {
	case ExprColumn:
		idx := table.ColumnIndex(e.Column)
		if idx < 0 {
			return nil, resolvedType{}, NewError(UndefinedColumn, "column does not exist: "+e.Column)
		}
		return &rExpr{kind: reColumn, index: idx},
			resolvedType{kind: rtInt, intTy: table.Columns[idx].Type}, nil
	case ExprLiteral:
		switch e.Literal.Kind {
		case LiteralNull:
			return &rExpr{kind: reConstNull}, resolvedType{kind: rtNull}, nil
		case LiteralBool:
			return &rExpr{kind: reConstBool, cBool: e.Literal.Bool}, resolvedType{kind: rtBool}, nil
		default: // LiteralInt
			ty := Int64
			if ctx != nil {
				ty = *ctx
			}
			if !ty.InRange(e.Literal.Int) {
				return nil, resolvedType{}, overflowErr(ty)
			}
			return &rExpr{kind: reConstInt, cInt: e.Literal.Int},
				resolvedType{kind: rtInt, intTy: ty}, nil
		}
	case ExprCast:
		target, err := resolveStorableType(e.Cast.TypeName)
		if err != nil {
			return nil, resolvedType{}, err
		}
		inner, ity, err := resolve(table, e.Cast.Inner, nil)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ity.kind == rtBool {
			return nil, resolvedType{}, typeError("cannot cast boolean to " + target.CanonicalName())
		}
		return &rExpr{kind: reCast, operand: inner, result: target},
			resolvedType{kind: rtInt, intTy: target}, nil
	case ExprUnary:
		if e.Unary.Op == OpNeg {
			rop, ty, err := resolve(table, e.Unary.Operand, ctx)
			if err != nil {
				return nil, resolvedType{}, err
			}
			result := Int64
			switch ty.kind {
			case rtInt:
				result = ty.intTy
			case rtNull:
				result = Int64 // -NULL = NULL
			default: // rtBool
				return nil, resolvedType{}, typeError("unary minus requires an integer operand")
			}
			return &rExpr{kind: reNeg, operand: rop, result: result},
				resolvedType{kind: rtInt, intTy: result}, nil
		}
		// OpNot
		rop, ty, err := resolve(table, e.Unary.Operand, nil)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireBool(ty, "NOT requires a boolean operand"); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reNot, operand: rop}, resolvedType{kind: rtBool}, nil
	case ExprIsNull:
		rop, _, err := resolve(table, e.IsNullOf.Operand, nil)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reIsNull, operand: rop, negated: e.IsNullOf.Negated},
			resolvedType{kind: rtBool}, nil
	case ExprIsDistinct:
		// NULL-safe equality: the SAME integer operand contract as `=` (promote a
		// mixed-width pair, adapt a literal to the sibling's type and range-check it).
		// The result is always a definite boolean (functions.md §3).
		rl, _, rr, _, err := resolveIntPair(table, e.IsDistinct.Lhs, e.IsDistinct.Rhs)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reDistinct, lhs: rl, rhs: rr, negated: e.IsDistinct.Negated},
			resolvedType{kind: rtBool}, nil
	default: // ExprBinary
		return resolveBinary(table, e.Binary)
	}
}

func resolveBinary(table *Table, b *BinaryExpr) (*rExpr, resolvedType, error) {
	switch b.Op {
	case OpAdd, OpSub, OpMul, OpDiv, OpMod:
		rl, lt, rr, rt, err := resolveIntPair(table, b.Lhs, b.Rhs)
		if err != nil {
			return nil, resolvedType{}, err
		}
		result := promote(lt, rt)
		return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: result},
			resolvedType{kind: rtInt, intTy: result}, nil
	case OpEq, OpLt, OpGt, OpLe, OpGe:
		rl, _, rr, _, err := resolveIntPair(table, b.Lhs, b.Rhs)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reCompare, op: b.Op, lhs: rl, rhs: rr},
			resolvedType{kind: rtBool}, nil
	default: // OpAnd, OpOr
		rl, lt, err := resolve(table, b.Lhs, nil)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rr, rt, err := resolve(table, b.Rhs, nil)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireBool(lt, "AND/OR requires boolean operands"); err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireBool(rt, "AND/OR requires boolean operands"); err != nil {
			return nil, resolvedType{}, err
		}
		kind := reAnd
		if b.Op == OpOr {
			kind = reOr
		}
		return &rExpr{kind: kind, lhs: rl, rhs: rr}, resolvedType{kind: rtBool}, nil
	}
}

// resolveIntPair resolves the two operands of an arithmetic/comparison operator, giving
// a bare integer literal the *other* operand's type as context (so `small + 1` types `1`
// as int16, and `small + 100000` traps 22003 at resolve). Both must be integer (or NULL);
// a boolean operand is a 42804 type error.
func resolveIntPair(table *Table, lhs, rhs Expr) (*rExpr, resolvedType, *rExpr, resolvedType, error) {
	lhsLit := isIntLiteral(lhs)
	rhsLit := isIntLiteral(rhs)
	var rl, rr *rExpr
	var lt, rt resolvedType
	var err error
	switch {
	case lhsLit && rhsLit:
		i64 := Int64
		if rl, lt, err = resolve(table, lhs, &i64); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(table, rhs, &i64)
	case lhsLit:
		if rr, rt, err = resolve(table, rhs, nil); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rl, lt, err = resolve(table, lhs, ctxOf(rt))
	case rhsLit:
		if rl, lt, err = resolve(table, lhs, nil); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(table, rhs, ctxOf(lt))
	default:
		if rl, lt, err = resolve(table, lhs, nil); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(table, rhs, nil)
	}
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	if err := requireIntOperand(lt); err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	if err := requireIntOperand(rt); err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	return rl, lt, rr, rt, nil
}

func isIntLiteral(e Expr) bool {
	return e.Kind == ExprLiteral && e.Literal.Kind == LiteralInt
}

// promote is the promotion-tower result type of two arithmetic operands: the
// higher-ranked integer type, or int64 when both are untyped NULLs.
func promote(a, b resolvedType) ScalarType {
	ax, aok := intType(a)
	bx, bok := intType(b)
	switch {
	case aok && bok:
		if ax.Rank() >= bx.Rank() {
			return ax
		}
		return bx
	case aok:
		return ax
	case bok:
		return bx
	default:
		return Int64
	}
}

func requireIntOperand(t resolvedType) error {
	if t.kind == rtBool {
		return typeError("arithmetic and comparison operators require integer operands")
	}
	return nil
}

func requireBool(t resolvedType, msg string) error {
	if t.kind == rtInt {
		return typeError(msg)
	}
	return nil
}

// requireAssignableInt: a value assigned to an integer column must itself be integer
// (or NULL); a boolean expression is a 42804 type error.
func requireAssignableInt(t resolvedType, col string) error {
	if t.kind == rtBool {
		return typeError("cannot assign a boolean value to integer column " + col)
	}
	return nil
}

// resolveStorableType resolves a column-definition or CAST target type name. Only the
// storable integer types are valid; boolean is known-but-not-storable (→ 0A000),
// distinct from a genuinely unknown name (→ 42704).
func resolveStorableType(name string) (ScalarType, error) {
	if ty, ok := ScalarTypeFromName(name); ok {
		return ty, nil
	}
	if IsBooleanTypeName(name) {
		return 0, NewError(FeatureNotSupported, "boolean is not a storable type yet: "+name)
	}
	return 0, NewError(UndefinedObject, "type does not exist: "+name)
}

func overflowErr(ty ScalarType) error {
	return NewError(NumericValueOutOfRange, "value out of range for type "+ty.CanonicalName())
}

func typeError(msg string) error { return NewError(DatatypeMismatch, msg) }

// eval evaluates against a row, accruing cost into m, and returns a Value (a boolean for
// comparisons / connectives). Arithmetic traps 22003 on overflow and 22012 on a zero
// divisor; NULL propagates through arithmetic; the connectives are Kleene; IS NULL is
// always definite.
//
// Cost: each INTERIOR node charges operator_eval once, pre-order (the node, then its
// operands LHS-before-RHS); leaf nodes (column/constants) charge nothing. Both operands
// are always evaluated — there is no short-circuit, so the count never depends on operand
// values (spec/design/cost.md §3).
func (e *rExpr) eval(row Row, m *Meter) (Value, error) {
	switch e.kind {
	case reColumn:
		return row[e.index], nil
	case reConstInt:
		return IntValue(e.cInt), nil
	case reConstBool:
		return BoolValue(e.cBool), nil
	case reConstNull:
		return NullValue(), nil
	case reCast:
		m.Charge(Costs.OperatorEval)
		v, err := e.operand.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		if !e.result.InRange(v.Int) {
			return Value{}, overflowErr(e.result)
		}
		return IntValue(v.Int), nil
	case reNeg:
		m.Charge(Costs.OperatorEval)
		v, err := e.operand.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		if v.Int == math.MinInt64 { // negating int64's minimum overflows int64
			return Value{}, overflowErr(e.result)
		}
		n := -v.Int
		if !e.result.InRange(n) {
			return Value{}, overflowErr(e.result)
		}
		return IntValue(n), nil
	case reNot:
		m.Charge(Costs.OperatorEval)
		v, err := e.operand.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		return boolNot(v), nil
	case reArith:
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		if a.Kind == ValNull || b.Kind == ValNull {
			return NullValue(), nil
		}
		return evalArith(e.op, a.Int, b.Int, e.result)
	case reCompare:
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		switch e.op {
		case OpEq:
			return from3(a.Eq3(b)), nil
		case OpLt:
			return from3(a.Lt3(b)), nil
		case OpGt:
			return from3(a.Gt3(b)), nil
		case OpLe:
			return from3(or3(a.Lt3(b), a.Eq3(b))), nil
		default: // OpGe
			return from3(or3(a.Gt3(b), a.Eq3(b))), nil
		}
	case reAnd:
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		return boolAnd(a, b), nil
	case reOr:
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		return boolOr(a, b), nil
	case reIsNull:
		m.Charge(Costs.OperatorEval)
		v, err := e.operand.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		isNull := v.Kind == ValNull
		return BoolValue(isNull != e.negated), nil
	default: // reDistinct
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, m)
		if err != nil {
			return Value{}, err
		}
		// negated carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks "are they
		// the same?"; IS DISTINCT FROM asks the opposite. Always a definite boolean — never
		// unknown (the null_safe discipline, functions.md §3).
		return BoolValue(a.NotDistinctFrom(b) == e.negated), nil
	}
}

// evalArith evaluates an integer arithmetic op in 64-bit, trapping 22012 on a zero
// divisor and 22003 if the op overflows int64 OR the in-range result falls outside the
// declared result type (the int16+int16 → int16 boundary — spec/design/functions.md §7).
func evalArith(op BinaryOp, x, y int64, result ScalarType) (Value, error) {
	var v int64
	switch op {
	case OpAdd:
		v = x + y
		if (y > 0 && v < x) || (y < 0 && v > x) {
			return Value{}, overflowErr(result)
		}
	case OpSub:
		v = x - y
		if (y < 0 && v < x) || (y > 0 && v > x) {
			return Value{}, overflowErr(result)
		}
	case OpMul:
		v = x * y
		if x != 0 && (v/x != y || (x == -1 && y == math.MinInt64)) {
			return Value{}, overflowErr(result)
		}
	case OpDiv:
		if y == 0 {
			return Value{}, NewError(DivisionByZero, "division by zero")
		}
		if x == math.MinInt64 && y == -1 {
			return Value{}, overflowErr(result)
		}
		v = x / y
	default: // OpMod
		if y == 0 {
			return Value{}, NewError(DivisionByZero, "division by zero")
		}
		if x == math.MinInt64 && y == -1 {
			return Value{}, overflowErr(result)
		}
		v = x % y
	}
	if !result.InRange(v) {
		return Value{}, overflowErr(result)
	}
	return IntValue(v), nil
}

// or3 is three-valued OR (Kleene): used to build <= / >= from < / > and =, so a NULL
// operand yields UNKNOWN rather than a wrong FALSE (CLAUDE.md §4).
func or3(a, b ThreeValued) ThreeValued {
	if a == True || b == True {
		return True
	}
	if a == Unknown || b == Unknown {
		return Unknown
	}
	return False
}

// nullFirstCmp is a total order for ORDER BY with NULLs first (ascending), matching the
// key encoding's physical order (spec/design/encoding.md §4). Returns <0, 0, >0. ORDER
// BY is over an (always-integer) column this slice, so the boolean ordering is defined
// (false < true) only for totality.
func nullFirstCmp(a, b Value) int {
	switch {
	case a.Kind == ValNull && b.Kind == ValNull:
		return 0
	case a.Kind == ValNull:
		return -1
	case b.Kind == ValNull:
		return 1
	}
	x, y := orderKey(a), orderKey(b)
	switch {
	case x < y:
		return -1
	case x > y:
		return 1
	default:
		return 0
	}
}

func orderKey(v Value) int64 {
	if v.Kind == ValBool {
		if v.Bool {
			return 1
		}
		return 0
	}
	return v.Int
}

// assignPlan is a resolved UPDATE assignment: the target column index, its type and
// nullability for re-checking, and the resolved RHS expression (evaluated against the
// old row).
type assignPlan struct {
	idx     int
	name    string
	target  ScalarType
	notNull bool
	source  *rExpr
}

// check type-checks a candidate value against this column: NULL into NOT NULL traps
// 23502; an integer outside the target range traps 22003 (CLAUDE.md §8) — mirrors
// INSERT's per-value checks. The resolver proved the value is integer or NULL, never
// boolean.
func (p assignPlan) check(v Value) (Value, error) {
	switch v.Kind {
	case ValNull:
		if p.notNull {
			return Value{}, NewError(NotNullViolation,
				"null value in column "+p.name+" violates not-null constraint")
		}
		return NullValue(), nil
	case ValInt:
		if !p.target.InRange(v.Int) {
			return Value{}, overflowErr(p.target)
		}
		return IntValue(v.Int), nil
	default: // ValBool — unreachable: resolver rejects assigning a boolean to a column
		return Value{}, NewError(FeatureNotSupported, "internal: boolean assigned to integer column")
	}
}
