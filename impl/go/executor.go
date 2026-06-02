package abide

import (
	"fmt"
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

// Outcome is the result of executing one statement.
type Outcome struct {
	Kind        OutcomeKind
	ColumnCount int
	Rows        [][]Value
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
		ty, ok := ScalarTypeFromName(def.TypeName)
		if !ok {
			return Outcome{}, NewError(UndefinedObject, "type does not exist: "+def.TypeName)
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
	return Outcome{Kind: OutcomeStatement}, nil
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
	return Outcome{Kind: OutcomeStatement}, nil
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
	var filter *resolvedPredicate
	if del.Filter != nil {
		var err error
		filter, err = db.resolvePredicate(table, del.Filter)
		if err != nil {
			return Outcome{}, err
		}
	}

	store := db.stores[strings.ToLower(del.Table)]
	var keys [][]byte
	for _, e := range store.EntriesInKeyOrder() {
		if filter == nil || filter.eval(e.Row).IsTrue() {
			keys = append(keys, e.Key)
		}
	}
	for _, k := range keys {
		store.Remove(k)
	}
	return Outcome{Kind: OutcomeStatement}, nil
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
		p := assignPlan{idx: idx, name: col.Name, target: col.Type, notNull: col.NotNull}
		if lit := a.Value.Literal; lit != nil {
			if lit.Kind == LiteralInt {
				p.constVal = IntValue(lit.Int)
			} else {
				p.constVal = NullValue()
			}
		} else {
			j := table.ColumnIndex(a.Value.Column)
			if j < 0 {
				return Outcome{}, NewError(UndefinedColumn, "column does not exist: "+a.Value.Column)
			}
			p.srcColumn = j
			p.srcIsColumn = true
		}
		plans = append(plans, p)
	}

	var filter *resolvedPredicate
	if upd.Filter != nil {
		var err error
		filter, err = db.resolvePredicate(table, upd.Filter)
		if err != nil {
			return Outcome{}, err
		}
	}

	// Phase 1: build + validate every matching row's new values; no writes yet.
	store := db.stores[strings.ToLower(upd.Table)]
	type pending struct {
		key []byte
		row Row
	}
	var updates []pending
	for _, e := range store.EntriesInKeyOrder() {
		if filter != nil && !filter.eval(e.Row).IsTrue() {
			continue
		}
		newRow := make(Row, len(e.Row))
		copy(newRow, e.Row)
		for _, p := range plans {
			raw := p.constVal
			if p.srcIsColumn {
				raw = e.Row[p.srcColumn]
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
	return Outcome{Kind: OutcomeStatement}, nil
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

	projections, err := db.resolveProjections(table, sel.Items)
	if err != nil {
		return Outcome{}, err
	}

	var filter *resolvedPredicate
	if sel.Filter != nil {
		filter, err = db.resolvePredicate(table, sel.Filter)
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

	// Scan in primary-key order, then filter.
	var rows []Row
	for _, row := range db.RowsInKeyOrder(sel.From) {
		if filter == nil || filter.eval(row).IsTrue() {
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

	// Project each surviving row.
	out := make([][]Value, 0, len(rows))
	for _, row := range rows {
		projected := make([]Value, len(projections))
		for i, p := range projections {
			v, err := p.eval(row)
			if err != nil {
				return Outcome{}, err
			}
			projected[i] = v
		}
		out = append(out, projected)
	}

	return Outcome{Kind: OutcomeQuery, ColumnCount: len(projections), Rows: out}, nil
}

func (db *Database) resolveProjections(table *Table, items SelectItems) ([]projection, error) {
	if items.All {
		ps := make([]projection, len(table.Columns))
		for i := range table.Columns {
			ps[i] = projection{kind: projColumn, index: i}
		}
		return ps, nil
	}
	ps := make([]projection, 0, len(items.Items))
	for _, e := range items.Items {
		p, err := db.resolveExpr(table, e)
		if err != nil {
			return nil, err
		}
		ps = append(ps, p)
	}
	return ps, nil
}

func (db *Database) resolveExpr(table *Table, e SelectExpr) (projection, error) {
	switch {
	case e.Cast != nil:
		target, ok := ScalarTypeFromName(e.Cast.TypeName)
		if !ok {
			return projection{}, NewError(UndefinedObject, "type does not exist: "+e.Cast.TypeName)
		}
		inner, err := db.resolveExpr(table, e.Cast.Inner)
		if err != nil {
			return projection{}, err
		}
		boxed := inner
		return projection{kind: projCast, target: target, inner: &boxed}, nil
	case e.Literal != nil:
		if e.Literal.Kind == LiteralNull {
			return projection{kind: projNull}, nil
		}
		return projection{kind: projLiteralInt, literal: e.Literal.Int}, nil
	default:
		idx := table.ColumnIndex(e.Column)
		if idx < 0 {
			return projection{}, NewError(UndefinedColumn, "column does not exist: "+e.Column)
		}
		return projection{kind: projColumn, index: idx}, nil
	}
}

func (db *Database) resolvePredicate(table *Table, p *Predicate) (*resolvedPredicate, error) {
	resolveCol := func(name string) (int, error) {
		idx := table.ColumnIndex(name)
		if idx < 0 {
			return 0, NewError(UndefinedColumn, "column does not exist: "+name)
		}
		return idx, nil
	}
	switch {
	case p.Compare != nil:
		idx, err := resolveCol(p.Compare.Column)
		if err != nil {
			return nil, err
		}
		rp := &resolvedPredicate{index: idx, op: p.Compare.Op}
		if lit := p.Compare.RHS.Literal; lit != nil {
			if lit.Kind == LiteralInt {
				// Context-adaptive literal (spec/design/types.md): the literal adapts to
				// the compared column's type; a value that does not fit traps 22003 here,
				// before any row is scanned (deterministic).
				ty := table.Columns[idx].Type
				if !ty.InRange(lit.Int) {
					return nil, NewError(NumericValueOutOfRange,
						"value out of range for type "+ty.CanonicalName())
				}
				rp.rhsConst = IntValue(lit.Int)
			} else {
				rp.rhsConst = NullValue()
			}
		} else {
			j, err := resolveCol(p.Compare.RHS.Column)
			if err != nil {
				return nil, err
			}
			rp.rhsColumn = j
			rp.rhsIsColumn = true
		}
		return rp, nil
	default: // IsNull
		idx, err := resolveCol(p.IsNull.Column)
		if err != nil {
			return nil, err
		}
		return &resolvedPredicate{isNull: true, index: idx, negated: p.IsNull.Negated}, nil
	}
}

// projKind tags a resolved projection.
type projKind int

const (
	projColumn projKind = iota
	projLiteralInt
	projNull
	projCast
)

// projection is a resolved output column: how to produce one value from a row.
type projection struct {
	kind    projKind
	index   int         // projColumn
	literal int64       // projLiteralInt
	target  ScalarType  // projCast
	inner   *projection // projCast
}

func (p projection) eval(row Row) (Value, error) {
	switch p.kind {
	case projColumn:
		return row[p.index], nil
	case projLiteralInt:
		return IntValue(p.literal), nil
	case projNull:
		return NullValue(), nil
	case projCast:
		v, err := p.inner.eval(row)
		if err != nil {
			return Value{}, err
		}
		if v.Null {
			return NullValue(), nil
		}
		if !p.target.InRange(v.Int) {
			return Value{}, NewError(NumericValueOutOfRange,
				"value out of range for type "+p.target.CanonicalName())
		}
		return IntValue(v.Int), nil
	default:
		return Value{}, NewError(FeatureNotSupported, "unknown projection")
	}
}

// resolvedPredicate is a WHERE predicate over fixed column indices.
type resolvedPredicate struct {
	isNull      bool // true => IS [NOT] NULL; false => comparison
	index       int
	op          CompareOp // comparison
	rhsConst    Value     // comparison, when rhsIsColumn is false
	rhsColumn   int       // comparison, when rhsIsColumn is true
	rhsIsColumn bool      // comparison: RHS is another column
	negated     bool      // IS NOT NULL
}

// eval returns a three-valued result; a WHERE clause keeps a row only on True.
func (p *resolvedPredicate) eval(row Row) ThreeValued {
	if p.isNull {
		got := row[p.index].Null != p.negated
		if got {
			return True
		}
		return False
	}
	lhs := row[p.index]
	rhs := p.rhsConst
	if p.rhsIsColumn {
		rhs = row[p.rhsColumn]
	}
	switch p.op {
	case OpEq:
		return lhs.Eq3(rhs)
	case OpLt:
		return lhs.Lt3(rhs)
	case OpGt:
		return lhs.Gt3(rhs)
	case OpLe:
		return or3(lhs.Lt3(rhs), lhs.Eq3(rhs))
	case OpGe:
		return or3(lhs.Gt3(rhs), lhs.Eq3(rhs))
	default:
		return Unknown
	}
}

// or3 is three-valued OR (Kleene): used to build <= / >= from < / > and =, so a
// NULL operand yields UNKNOWN rather than a wrong FALSE (CLAUDE.md §4).
func or3(a, b ThreeValued) ThreeValued {
	if a == True || b == True {
		return True
	}
	if a == Unknown || b == Unknown {
		return Unknown
	}
	return False
}

// nullFirstCmp is a total order for ORDER BY with NULLs first (ascending), matching
// the key encoding's physical order (spec/design/encoding.md §4). Returns <0, 0, >0.
func nullFirstCmp(a, b Value) int {
	switch {
	case a.Null && b.Null:
		return 0
	case a.Null:
		return -1
	case b.Null:
		return 1
	case a.Int < b.Int:
		return -1
	case a.Int > b.Int:
		return 1
	default:
		return 0
	}
}

// assignPlan is a resolved UPDATE assignment: the target column index, its type and
// nullability for re-checking, and the value source (a constant, or another column
// read from the same row when srcIsColumn).
type assignPlan struct {
	idx         int
	name        string
	target      ScalarType
	notNull     bool
	constVal    Value
	srcColumn   int
	srcIsColumn bool
}

// check type-checks a candidate value against this column: NULL into NOT NULL traps
// 23502; an integer outside the target range traps 22003 (CLAUDE.md §8) — mirrors
// INSERT's per-value checks.
func (p assignPlan) check(v Value) (Value, error) {
	if v.Null {
		if p.notNull {
			return Value{}, NewError(NotNullViolation,
				"null value in column "+p.name+" violates not-null constraint")
		}
		return NullValue(), nil
	}
	if !p.target.InRange(v.Int) {
		return Value{}, NewError(NumericValueOutOfRange,
			"value out of range for type "+p.target.CanonicalName())
	}
	return IntValue(v.Int), nil
}
