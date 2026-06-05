package jed

import (
	"fmt"
	"math"
	"sort"
	"strconv"
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
	Kind OutcomeKind
	// ColumnNames are the output column names of a query result (nil for a non-query
	// statement); the column count is len(ColumnNames) (spec/design/grammar.md §8).
	ColumnNames []string
	Rows        [][]Value
	Cost        int64
}

// DefaultPageSize is the default serialization page size (8 KiB — spec/design/storage.md §3),
// used for a fresh in-memory or newly-created database when no explicit size is given.
const DefaultPageSize uint32 = 8192

// Database is the whole database: catalog + per-table in-memory stores. Single
// committed state (CLAUDE.md §3); the staging-buffer commit model lands with
// persistence.
type Database struct {
	tables map[string]*Table
	stores map[string]*TableStore
	// path is the backing file (empty for an in-memory database). Set by the host API
	// Open/Create (spec/design/api.md §2); Commit writes here.
	path string
	// txid is the monotonic commit counter: read from the file on open, bumped per Commit.
	txid uint64
	// pageSize is the page size this database serializes with (fixed for the life of a file).
	pageSize uint32
}

// NewDatabase builds an empty in-memory database.
func NewDatabase() *Database {
	return &Database{
		tables:   make(map[string]*Table),
		stores:   make(map[string]*TableStore),
		pageSize: DefaultPageSize,
	}
}

// Txid is the monotonic commit counter (spec/design/api.md §2).
func (db *Database) Txid() uint64 { return db.txid }

// PageSize is the page size this database serializes with (spec/design/api.md §2).
func (db *Database) PageSize() uint32 { return db.pageSize }

// Path is the backing file path, or "" for an in-memory database.
func (db *Database) Path() string { return db.path }

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

// ExecuteStmt executes one parsed statement with no bind parameters.
func (db *Database) ExecuteStmt(stmt Statement) (Outcome, error) {
	return db.ExecuteStmtParams(stmt, nil)
}

// ExecuteStmtParams executes one parsed statement, binding params to its $N placeholders (nil
// for an unparameterized statement). DDL statements take no parameters — supplying any is a
// 42601 (spec/design/api.md §5).
func (db *Database) ExecuteStmtParams(stmt Statement, params []Value) (Outcome, error) {
	switch {
	case stmt.CreateTable != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return Outcome{}, err
		}
		return db.executeCreateTable(stmt.CreateTable)
	case stmt.DropTable != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return Outcome{}, err
		}
		return db.executeDropTable(stmt.DropTable)
	case stmt.Insert != nil:
		return db.executeInsert(stmt.Insert, params)
	case stmt.Select != nil:
		return db.executeSelect(stmt.Select, params)
	case stmt.Update != nil:
		return db.executeUpdate(stmt.Update, params)
	case stmt.Delete != nil:
		return db.executeDelete(stmt.Delete, params)
	default:
		return Outcome{}, NewError(SyntaxError, "empty statement")
	}
}

// rejectParamsForDDL errors (42601) if bind parameters are supplied to a CREATE/DROP TABLE
// (which has no expressions to bind — spec/design/api.md §5).
func rejectParamsForDDL(params []Value) error {
	if len(params) > 0 {
		return NewError(SyntaxError, "bind parameters are not allowed in a DDL statement")
	}
	return nil
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
		ty, decimal, err := resolveTypeAndTypmod(def.TypeName, def.TypeMod)
		if err != nil {
			return Outcome{}, err
		}
		if def.PrimaryKey {
			// Integers and uuid may be a key. uuid is the FIRST non-integer key type — its
			// fixed uuid-raw16 encoding (spec/design/encoding.md §2.7) is exercised. The other
			// non-integer types' order-preserving key encodings (text §2.4, decimal §2.5,
			// bytea §2.6, boolean's bool-byte) are authored but unexercised, so a
			// text/decimal/bytea/boolean PRIMARY KEY is a documented 0A000 narrowing
			// (types.md §9/§11/§12/§13), relaxable in a later in-key slice.
			if !ty.IsInteger() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() {
				return Outcome{}, NewError(FeatureNotSupported,
					"a "+ty.CanonicalName()+" primary key is not supported yet")
			}
			if pkSeen {
				return Outcome{}, NewError(InvalidTableDefinition,
					"a table may have at most one primary key")
			}
			pkSeen = true
		}
		// Evaluate + type-coerce the DEFAULT literal once, here. A bad default fails at CREATE
		// TABLE: out of range 22003, cross-family 42804, decimal over-precision 22003. NOT NULL
		// is NOT enforced here (notNull=false), so a DEFAULT NULL on a NOT NULL column is
		// accepted and traps 23502 only when applied (constraints.md §2).
		var defaultVal *Value
		if def.Default != nil {
			dv, err := storeValue(literalToValue(*def.Default), ty, decimal, false, def.Name)
			if err != nil {
				return Outcome{}, err
			}
			defaultVal = &dv
		}
		columns = append(columns, Column{
			Name:       def.Name,
			Type:       ty,
			Decimal:    decimal,
			PrimaryKey: def.PrimaryKey,
			NotNull:    def.PrimaryKey || def.NotNull, // PRIMARY KEY ⇒ NOT NULL
			Default:    defaultVal,
		})
	}

	db.putTable(&Table{Name: ct.Name, Columns: columns})
	// DDL touches no rows and evaluates no expressions: zero cost.
	return Outcome{Kind: OutcomeStatement, Cost: 0}, nil
}

// executeDropTable runs a DROP TABLE: remove the table's definition and its row store
// from the catalog (both keyed by the lower-cased name). A table that does not exist is
// the same 42P01 the DML paths raise — there is no IF EXISTS this slice
// (spec/design/grammar.md §13). Like CREATE TABLE it touches no rows and evaluates no
// expression tree (the store is discarded wholesale), so it accrues zero cost.
func (db *Database) executeDropTable(dt *DropTable) (Outcome, error) {
	if _, ok := db.Table(dt.Name); !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+dt.Name)
	}
	key := strings.ToLower(dt.Name)
	delete(db.tables, key)
	delete(db.stores, key)
	return Outcome{Kind: OutcomeStatement, Cost: 0}, nil
}

// executeInsert analyzes and runs an INSERT of one or more rows. An optional column list names
// the target columns (unknown → 42703, duplicate → 42701); an unlisted column, or a DEFAULT
// keyword slot, takes the column's stored default else NULL. Each value is type-checked (NULL
// into NOT NULL traps 23502; an integer outside the column type's range traps 22003 — CLAUDE.md
// §8); a duplicate primary key traps 23505. A multi-row INSERT is two-phase / all-or-nothing
// (spec/design/grammar.md §12, constraints.md §2), mirroring UPDATE: every row is validated —
// including its storage key — before any row is inserted, so a mid-batch failure stores nothing.
func (db *Database) executeInsert(ins *Insert, params []Value) (Outcome, error) {
	table, ok := db.Table(ins.Table)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+ins.Table)
	}
	store := db.stores[strings.ToLower(ins.Table)]
	pk := table.PrimaryKeyIndex()

	// Resolve the optional column list once. provided[i] >= 0 means table column i takes that
	// value position in each row; -1 means column i is omitted (its default, else NULL). With no
	// list it is the identity over all columns. arity is how many values each row must carry.
	n := len(table.Columns)
	provided := make([]int, n)
	arity := n
	if ins.Columns != nil {
		for i := range provided {
			provided[i] = -1
		}
		for p, name := range ins.Columns {
			idx := table.ColumnIndex(name)
			if idx < 0 {
				return Outcome{}, NewError(UndefinedColumn, fmt.Sprintf(
					"column %s of relation %s does not exist", name, table.Name,
				))
			}
			if provided[idx] >= 0 {
				return Outcome{}, NewError(DuplicateColumn,
					"column "+table.Columns[idx].Name+" specified more than once")
			}
			provided[idx] = p
		}
		arity = len(ins.Columns)
	} else {
		for i := range provided {
			provided[i] = i
		}
	}

	// A $N in a VALUES slot is typed as its TARGET COLUMN's type. Collect those types across
	// every row (a $N reused under two columns unifies; spec/design/api.md §5), then bind the
	// supplied values up front so a bad bind fails before any row is stored.
	ptypes := &paramTypes{}
	for _, values := range ins.Rows {
		for i, col := range table.Columns {
			if p := provided[i]; p >= 0 && p < len(values) {
				if iv := values[p]; iv.IsParam {
					ct := col.Type
					if err := ptypes.note(int(iv.Param)-1, &ct); err != nil {
						return Outcome{}, err
					}
				}
			}
		}
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return Outcome{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return Outcome{}, err
	}

	// Phase 1 — validate every row and compute its key. Nothing is stored yet. For a
	// table with a primary key, the encoded key is checked for a duplicate (within the
	// batch via seenKeys, and against the store) up front; for a table with none, key is
	// left nil and a fresh monotonic rowid is allocated in phase 2.
	type preparedRow struct {
		key []byte // nil for a no-PK table (rowid allocated in phase 2)
		row Row
	}
	prepared := make([]preparedRow, 0, len(ins.Rows))
	seenKeys := make(map[string]struct{})
	for _, values := range ins.Rows {
		if len(values) != arity {
			expected := "columns are"
			if ins.Columns != nil {
				expected = "target columns are"
			}
			return Outcome{}, NewError(SyntaxError, fmt.Sprintf(
				"INSERT row has %d values but %d %s expected for table %s",
				len(values), arity, expected, table.Name,
			))
		}

		// Build the row in declaration order: each column takes its provided value (a literal,
		// or a DEFAULT keyword → the column default else NULL), or — when the column is omitted
		// — its default else NULL. storeValue then type-coerces and enforces NOT NULL (23502)
		// uniformly, so a NULL into a NOT NULL column traps here, before key encoding.
		row := make(Row, n)
		for i, col := range table.Columns {
			var candidate Value
			if p := provided[i]; p >= 0 {
				switch iv := values[p]; {
				case iv.IsDefault:
					candidate = defaultOrNull(col)
				case iv.IsParam:
					// A bound $N value; its target-column coercion is the storeValue below,
					// identical to a literal in this slot (spec/design/api.md §5).
					candidate = bound[int(iv.Param)-1]
				default:
					candidate = literalToValue(iv.Lit)
				}
			} else {
				candidate = defaultOrNull(col)
			}
			v, err := storeValue(candidate, col.Type, col.Decimal, col.NotNull, col.Name)
			if err != nil {
				return Outcome{}, err
			}
			row[i] = v
		}

		var key []byte
		if pk >= 0 {
			if table.Columns[pk].Type.IsUuid() {
				// uuid is the first non-integer key: its key is the bare 16 bytes (uuid-raw16,
				// encoding.md §2.7) — a PK is NOT NULL, so no presence tag, no sign-flip.
				key = []byte(row[pk].Str)
			} else {
				key = EncodeInt(table.Columns[pk].Type, row[pk].Int)
			}
			if _, dup := seenKeys[string(key)]; dup {
				return Outcome{}, NewError(UniqueViolation,
					"duplicate key value violates primary key uniqueness")
			}
			if _, exists := store.Get(key); exists {
				return Outcome{}, NewError(UniqueViolation,
					"duplicate key value violates primary key uniqueness")
			}
			seenKeys[string(key)] = struct{}{}
		}
		prepared = append(prepared, preparedRow{key: key, row: row})
	}

	// Phase 2 — every row validated, so each insert is guaranteed to succeed. A synthetic
	// rowid is allocated here, in row order, so a failed validation pass burns none
	// (spec/fileformat/format.md, spec/design/grammar.md §12).
	for _, pr := range prepared {
		key := pr.key
		if key == nil {
			key = EncodeInt(Int64, store.AllocRowid())
		}
		if !store.Insert(key, pr.row) {
			panic("pre-validated INSERT key must be unique")
		}
	}
	// INSERT reads no rows and evaluates no expression tree — its values are literals and
	// pre-evaluated constant defaults (folded at CREATE TABLE), i.e. leaves: zero cost.
	return Outcome{Kind: OutcomeStatement, Cost: 0}, nil
}

// defaultOrNull is the column's stored default value, or a NULL value when it has none —
// the candidate for an omitted column or a DEFAULT keyword slot (constraints.md §2).
func defaultOrNull(col Column) Value {
	if col.Default != nil {
		return *col.Default
	}
	return NullValue()
}

// executeDelete analyzes and runs a DELETE: resolve the table and optional predicate,
// collect the keys of matching rows (only a TRUE predicate matches — Kleene), then
// remove them. No WHERE deletes every row. Keys are collected before mutating so the
// map is not modified while iterating.
func (db *Database) executeDelete(del *Delete, params []Value) (Outcome, error) {
	table, ok := db.Table(del.Table)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+del.Table)
	}
	// DELETE is single-table; resolve its WHERE against a one-relation scope.
	s := singleScope(table)
	ptypes := &paramTypes{}
	var filter *rExpr
	if del.Filter != nil {
		f, err := resolveBooleanFilter(s, del.Filter, ptypes)
		if err != nil {
			return Outcome{}, err
		}
		filter = f
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return Outcome{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return Outcome{}, err
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
			v, err := filter.eval(e.Row, bound, meter)
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
func (db *Database) executeUpdate(upd *Update, params []Value) (Outcome, error) {
	table, ok := db.Table(upd.Table)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+upd.Table)
	}
	// UPDATE is single-table; the RHS / WHERE resolve against a one-relation scope so the
	// shared resolver serves it too (a qualified `WHERE t.a` against the sole table is fine).
	s := singleScope(table)
	ptypes := &paramTypes{}

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
		// The RHS is a general expression evaluated against the *old* row; a literal operand
		// adapts to the target column's type. The result must be assignable to the column's
		// family (integer/decimal/text or NULL; never boolean; decimal→int is explicit only).
		src, ty, err := resolve(s, a.Value, &col.Type, &aggCtx{collecting: false}, ptypes)
		if err != nil {
			return Outcome{}, err
		}
		if err := requireAssignable(ty, col.Type, a.Column); err != nil {
			return Outcome{}, err
		}
		plans = append(plans, assignPlan{
			idx: idx, name: col.Name, target: col.Type, decimal: col.Decimal, notNull: col.NotNull, source: src,
		})
	}

	var filter *rExpr
	if upd.Filter != nil {
		f, err := resolveBooleanFilter(s, upd.Filter, ptypes)
		if err != nil {
			return Outcome{}, err
		}
		filter = f
	}
	// All assignment RHSs + the WHERE are resolved: finalize + bind before any scan.
	ptys, err := ptypes.finalize()
	if err != nil {
		return Outcome{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return Outcome{}, err
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
			v, err := filter.eval(e.Row, bound, meter)
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
			raw, err := p.source.eval(e.Row, bound, meter)
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
func (db *Database) executeSelect(sel *Select, params []Value) (Outcome, error) {
	// Accumulates the inferred type of each $N across every clause of this SELECT, then is
	// finalized + bound once all resolution is done (spec/design/api.md §5).
	ptypes := &paramTypes{}
	// Build the FROM scope: resolve each table reference (42P01 if unknown), compute each
	// relation's flat column offset in FROM order, and reject a duplicate label — a self-join
	// without distinct aliases is 42712 (spec/design/grammar.md §15).
	tableRefs := make([]TableRef, 0, 1+len(sel.Joins))
	tableRefs = append(tableRefs, sel.From)
	for _, j := range sel.Joins {
		tableRefs = append(tableRefs, j.Table)
	}
	var rels []scopeRel
	seenLabels := make(map[string]bool)
	offset := 0
	for _, tref := range tableRefs {
		t, ok := db.Table(tref.Name)
		if !ok {
			return Outcome{}, NewError(UndefinedTable, "table does not exist: "+tref.Name)
		}
		label := strings.ToLower(t.Name)
		if tref.Alias != nil {
			label = strings.ToLower(*tref.Alias)
		}
		if seenLabels[label] {
			return Outcome{}, NewError(DuplicateAlias, "table name "+label+" specified more than once")
		}
		seenLabels[label] = true
		rels = append(rels, scopeRel{label: label, table: t, offset: offset})
		offset += len(t.Columns)
	}
	s := &scope{rels: rels}

	// Resolve projections (paired with output names — §8), the optional WHERE (must be
	// boolean), and the ORDER BY keys against the full scope. A bare key ambiguous across
	// relations is 42702; an unknown qualifier is 42P01 (§15).
	// Resolve GROUP BY keys to flat row indices (a key is a bare/qualified column — grammar.md
	// §18). An unknown column is 42703, an ambiguous bare key 42702.
	var err error
	groupKeys := make([]int, 0, len(sel.GroupBy))
	for _, key := range sel.GroupBy {
		var idx int
		if key.Kind == ExprQualifiedColumn {
			idx, err = s.resolveQualified(key.Qualifier, key.Column)
		} else {
			idx, err = s.resolveBare(key.Column)
		}
		if err != nil {
			return Outcome{}, err
		}
		groupKeys = append(groupKeys, idx)
	}

	// An aggregate query has a GROUP BY or an aggregate in the select list. Its projection
	// resolves in collect mode — aggregates collect into synthetic slots and a non-grouped
	// column is 42803 (spec/design/aggregates.md §4/§6); a plain query resolves in Forbidden
	// mode (columns normal). Output names per grammar.md §8.
	// GROUP BY, an aggregate in the select list, OR a HAVING clause all make this an aggregate
	// query (HAVING alone groups the whole table — grammar.md §19).
	isAgg := len(groupKeys) > 0 || itemsHaveAggregate(sel.Items) || sel.Having != nil
	projAgg := &aggCtx{collecting: isAgg, groupKeys: groupKeys}
	projections, columnNames, err := resolveProjections(s, sel.Items, projAgg, ptypes)
	if err != nil {
		return Outcome{}, err
	}
	// HAVING resolves against the same grouped scope (collect) — it may reference aggregates
	// (collected into the SAME specs, so their slots follow the projection's) and grouping keys;
	// a non-grouped column is 42803. It must be boolean (42804). Resolved after the projection so
	// the synthetic row is [group_keys..., projection aggs..., HAVING aggs...].
	var having *rExpr
	if sel.Having != nil {
		node, ty, herr := resolve(s, *sel.Having, nil, projAgg, ptypes)
		if herr != nil {
			return Outcome{}, herr
		}
		if ty.kind != rtBool && ty.kind != rtNull {
			return Outcome{}, typeError("argument of HAVING must be boolean")
		}
		having = node
	}
	aggSpecs := projAgg.specs
	// SELECT DISTINCT over an aggregate query's output (output-row dedup) is deferred (0A000).
	if isAgg && sel.Distinct {
		return Outcome{}, NewError(FeatureNotSupported, "SELECT DISTINCT with aggregates is not supported yet")
	}
	var filter *rExpr
	if sel.Filter != nil {
		filter, err = resolveBooleanFilter(s, sel.Filter, ptypes)
		if err != nil {
			return Outcome{}, err
		}
	}
	type orderKeyPlan struct {
		idx        int
		descending bool
		nullsFirst bool
	}
	// ORDER BY resolution. In an aggregate query a key resolves against the GROUP KEYS — a
	// grouping column gives its synthetic-row slot, a non-grouping column is 42803 (the
	// grouping-error rule, grammar.md §18); the sort runs on the group rows. In a plain query
	// keys resolve against the FROM scope (a flat row index).
	order := make([]orderKeyPlan, 0, len(sel.OrderBy))
	for _, key := range sel.OrderBy {
		var idx int
		if key.Qualifier != "" {
			idx, err = s.resolveQualified(key.Qualifier, key.Column)
		} else {
			idx, err = s.resolveBare(key.Column)
		}
		if err != nil {
			return Outcome{}, err
		}
		slot := idx
		if isAgg {
			slot = -1
			for pos, gk := range groupKeys {
				if gk == idx {
					slot = pos
					break
				}
			}
			if slot < 0 {
				return Outcome{}, groupingErrorColumn(key.Column)
			}
		}
		order = append(order, orderKeyPlan{idx: slot, descending: key.Descending, nullsFirst: key.NullsFirst})
	}

	// SELECT DISTINCT restriction (spec/design/grammar.md §11): each ORDER BY key must appear
	// as a bare/qualified column in the select list (resolved to the same flat index; or the
	// list is `*`). Matches PostgreSQL (42P10). Aliases are invisible to ORDER BY (§8).
	if sel.Distinct && len(order) > 0 && !sel.Items.All {
		projected := make(map[int]bool)
		for _, it := range sel.Items.Items {
			switch it.Expr.Kind {
			case ExprColumn:
				if idx, e := s.resolveBare(it.Expr.Column); e == nil {
					projected[idx] = true
				}
			case ExprQualifiedColumn:
				if idx, e := s.resolveQualified(it.Expr.Qualifier, it.Expr.Column); e == nil {
					projected[idx] = true
				}
			}
		}
		for _, key := range order {
			if !projected[key.idx] {
				return Outcome{}, NewError(InvalidColumnReference,
					"for SELECT DISTINCT, ORDER BY expressions must appear in select list")
			}
		}
	}

	// Resolve each JOIN's ON predicate against the PARTIAL scope visible at that node (the
	// relations joined so far — rels[:k+2]), so a forward reference to a not-yet-joined table
	// is a clean 42P01/42703 instead of an out-of-range row index. CROSS has no ON; INNER and
	// the OUTER kinds (LEFT/RIGHT/FULL) all resolve their ON the same way — the join kind only
	// changes how unmatched rows are handled in the loop below (§15).
	joinOns := make([]*rExpr, len(sel.Joins))
	for k, j := range sel.Joins {
		if j.On != nil {
			partial := &scope{rels: s.rels[:k+2]}
			on, oerr := resolveBooleanFilter(partial, j.On, ptypes)
			if oerr != nil {
				return Outcome{}, oerr
			}
			joinOns[k] = on
		}
	}

	// All clauses resolved: finalize the inferred parameter types and bind the supplied values
	// (count mismatch 42601; out-of-range/family errors 22003/42804) BEFORE scanning any rows
	// (spec/design/api.md §5).
	ptys, err := ptypes.finalize()
	if err != nil {
		return Outcome{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return Outcome{}, err
	}

	// Materialize each base table once, in primary-key order, charging storage_row_read per
	// physical row (spec/design/cost.md §3 JOIN). The nested loop re-reads from these in-memory
	// buffers, which are not stores and charge nothing.
	meter := NewMeter()
	materialized := make([][]Row, len(s.rels))
	for ri, rel := range s.rels {
		var tableRows []Row
		for _, row := range db.RowsInKeyOrder(rel.table.Name) {
			meter.Charge(Costs.StorageRowRead)
			tableRows = append(tableRows, row)
		}
		materialized[ri] = tableRows
	}

	// Left-deep nested-loop join. `running` holds the combined rows over the relations joined
	// so far (starting with the first table's rows). For each join, concatenate every running
	// row with every right-table row; CROSS keeps all pairs, INNER keeps a pair iff its ON
	// predicate is TRUE (three-valued — a NULL join key never matches). LEFT/FULL additionally
	// emit each unmatched left row NULL-extended over the right side; RIGHT/FULL emit each
	// unmatched right row NULL-extended over the left side. The NULL-extension appends evaluate
	// no ON (no operator_eval — spec/design/cost.md §3). Output order is deterministic: running
	// order (outer) then right key order (inner), each unmatched left row after its (empty)
	// match run, all unmatched right rows last in right key order (CLAUDE.md §10).
	running := materialized[0]
	for k := range sel.Joins {
		rightRows := materialized[k+1]
		on := joinOns[k]
		emitLeft := sel.Joins[k].Kind == JoinLeft || sel.Joins[k].Kind == JoinFull
		emitRight := sel.Joins[k].Kind == JoinRight || sel.Joins[k].Kind == JoinFull
		// NULL-pad widths come from the SCOPE, never a sampled row, so they are correct even when
		// `running`/`rightRows` is empty: the right table begins at flat offset rels[k+1].offset
		// (= the width of every running row) and is that many columns wide.
		leftPad := s.rels[k+1].offset
		rightPad := len(s.rels[k+1].table.Columns)
		var next []Row
		rightMatched := make([]bool, len(rightRows))
		for _, left := range running {
			leftMatched := false
			for ri, right := range rightRows {
				combined := make(Row, 0, len(left)+len(right))
				combined = append(combined, left...)
				combined = append(combined, right...)
				keep := true
				if on != nil {
					v, err := on.eval(combined, bound, meter)
					if err != nil {
						return Outcome{}, err
					}
					keep = v.IsTrue()
				}
				if keep {
					next = append(next, combined)
					leftMatched = true
					rightMatched[ri] = true
				}
			}
			if emitLeft && !leftMatched {
				combined := make(Row, 0, len(left)+rightPad)
				combined = append(combined, left...)
				for i := 0; i < rightPad; i++ {
					combined = append(combined, NullValue())
				}
				next = append(next, combined)
			}
		}
		if emitRight {
			for ri, right := range rightRows {
				if !rightMatched[ri] {
					combined := make(Row, 0, leftPad+len(right))
					for i := 0; i < leftPad; i++ {
						combined = append(combined, NullValue())
					}
					combined = append(combined, right...)
					next = append(next, combined)
				}
			}
		}
		running = next
	}

	// WHERE over the combined rows. A WHERE arithmetic can trap (22003/22012); each surviving
	// combined row's filter accrues operator_eval.
	var rows []Row
	for _, row := range running {
		keep := true
		if filter != nil {
			v, err := filter.eval(row, bound, meter)
			if err != nil {
				return Outcome{}, err
			}
			keep = v.IsTrue()
		}
		if keep {
			rows = append(rows, row)
		}
	}

	// ORDER BY: stable sort applying each key left to right — the first non-equal key decides,
	// and a full tie keeps the scan order (SliceStable). Each key's NULL placement is decoupled
	// from its value-direction flip (spec/design/grammar.md §10). Aggregate queries sort their
	// GROUP rows in the aggregate branch below — not these pre-aggregation rows — so this is
	// gated to plain queries.
	if !isAgg && len(order) > 0 {
		sort.SliceStable(rows, func(a, b int) bool {
			for _, key := range order {
				c := keyCmp(rows[a][key.idx], rows[b][key.idx], key.descending, key.nullsFirst)
				if c != 0 {
					return c < 0
				}
			}
			return false
		})
	}

	// LIMIT / OFFSET window bounds over a result of n rows. Clamp in the int64 domain
	// against the row count before indexing — never truncate a huge count (CLAUDE.md §8;
	// spec/design/grammar.md §9). The counts are already non-negative (parser).
	windowBounds := func(n int64) (int64, int64) {
		start := int64(0)
		if sel.Offset != nil && *sel.Offset < n {
			start = *sel.Offset
		} else if sel.Offset != nil {
			start = n
		}
		end := n
		if sel.Limit != nil && *sel.Limit < n-start {
			end = start + *sel.Limit
		}
		return start, end
	}

	// Build the output rows. The two paths differ in pipeline order
	// (spec/design/grammar.md §11): without DISTINCT the window slices the sorted source
	// rows and ONLY the windowed rows are projected; with DISTINCT every (sorted) filtered
	// row is projected — dedup must see them all — duplicates drop by first occurrence, and
	// the window then slices the DISTINCT rows.
	var out [][]Value
	if isAgg {
		// Aggregate query — group + accumulate (aggregates.md §5). Bucket the post-WHERE rows by
		// their group-key values; the bucket key is the value-canonical distinctRowKey (it
		// collapses 1.5/1.50 and groups NULL with NULL), and the map is only an index — output
		// order comes from the insertion-ordered `groups`, never map iteration (no map-order leak
		// — CLAUDE.md §8/§10). Whole-table aggregation (no GROUP BY) is one pre-created empty-key
		// group, so it emits ONE row even over zero input; GROUP BY over an empty table creates no
		// groups -> zero rows. Each (row × aggregate) charges aggregate_accumulate; the operand's
		// own operator_evals accrue via eval; the bucketing/finalize is unmetered (cost.md §3).
		type group struct {
			keys []Value
			accs []*acc
		}
		newAccs := func() []*acc {
			a := make([]*acc, len(aggSpecs))
			for i, spec := range aggSpecs {
				a[i] = newAcc(spec.plan)
			}
			return a
		}
		index := make(map[string]int)
		var groups []group
		if len(groupKeys) == 0 {
			groups = append(groups, group{keys: nil, accs: newAccs()})
			index[""] = 0
		}
		for _, row := range rows {
			keys := make([]Value, len(groupKeys))
			for i, gk := range groupKeys {
				keys[i] = row[gk]
			}
			k := distinctRowKey(keys)
			gi, ok := index[k]
			if !ok {
				gi = len(groups)
				index[k] = gi
				groups = append(groups, group{keys: keys, accs: newAccs()})
			}
			for i, spec := range aggSpecs {
				meter.Charge(Costs.AggregateAccumulate)
				v := NullValue() // COUNT(*) ignores the value
				if spec.operand != nil {
					var verr error
					if v, verr = spec.operand.eval(row, bound, meter); verr != nil {
						return Outcome{}, verr
					}
				}
				if ferr := groups[gi].accs[i].fold(v); ferr != nil {
					return Outcome{}, ferr
				}
			}
		}
		// Build one synthetic row per group: [group_key_values..., aggregate_results...].
		groupRows := make([][]Value, 0, len(groups))
		for _, g := range groups {
			srow := make([]Value, 0, len(g.keys)+len(g.accs))
			srow = append(srow, g.keys...)
			for _, a := range g.accs {
				v, ferr := a.finalize()
				if ferr != nil {
					return Outcome{}, ferr
				}
				srow = append(srow, v)
			}
			groupRows = append(groupRows, srow)
		}
		// HAVING: filter the grouped rows (after aggregation, before ORDER BY). The predicate is
		// evaluated against each group's synthetic row (charging its operator_evals per group);
		// only a TRUE result keeps the group. A dropped group charges no row_produced (§8).
		if having != nil {
			kept := groupRows[:0:0]
			for _, srow := range groupRows {
				v, herr := having.eval(srow, bound, meter)
				if herr != nil {
					return Outcome{}, herr
				}
				if v.IsTrue() {
					kept = append(kept, srow)
				}
			}
			groupRows = kept
		}
		// ORDER BY over the grouped output (keys are synthetic group-key slots).
		if len(order) > 0 {
			sort.SliceStable(groupRows, func(a, b int) bool {
				for _, key := range order {
					c := keyCmp(groupRows[a][key.idx], groupRows[b][key.idx], key.descending, key.nullsFirst)
					if c != 0 {
						return c < 0
					}
				}
				return false
			})
		}
		// Window + project; only an emitted row charges row_produced + its projection cost.
		start, end := windowBounds(int64(len(groupRows)))
		out = make([][]Value, 0, end-start)
		for _, srow := range groupRows[start:end] {
			meter.Charge(Costs.RowProduced)
			projected := make([]Value, len(projections))
			for i, p := range projections {
				v, perr := p.eval(srow, bound, meter)
				if perr != nil {
					return Outcome{}, perr
				}
				projected[i] = v
			}
			out = append(out, projected)
		}
	} else if sel.Distinct {
		// Project every filtered row (charging projection cost per row, the §3 asymmetry),
		// keeping first occurrences. `seen` is membership-only: output order comes from the
		// deterministic source iteration, never from map iteration (no map-order leak —
		// CLAUDE.md §8/§10).
		seen := make(map[string]bool)
		var distinctRows [][]Value
		for _, row := range rows {
			projected := make([]Value, len(projections))
			for i, p := range projections {
				v, err := p.eval(row, bound, meter)
				if err != nil {
					return Outcome{}, err
				}
				projected[i] = v
			}
			if key := distinctRowKey(projected); !seen[key] {
				seen[key] = true
				distinctRows = append(distinctRows, projected)
			}
		}
		// LIMIT / OFFSET applies to the DISTINCT rows; only the emitted rows charge
		// RowProduced (spec/design/cost.md §3).
		start, end := windowBounds(int64(len(distinctRows)))
		out = make([][]Value, 0, end-start)
		for _, row := range distinctRows[start:end] {
			meter.Charge(Costs.RowProduced)
			out = append(out, row)
		}
	} else {
		// Window the sorted rows BEFORE projection, so rows skipped by OFFSET or excluded by
		// LIMIT accrue no row_produced/projection cost (they were still scanned + filtered
		// above). Producing a row, and each projection-list evaluation, accrue cost.
		// (ORDER BY's sort comparisons are not metered — spec/design/cost.md §3.)
		start, end := windowBounds(int64(len(rows)))
		windowed := rows[start:end]
		out = make([][]Value, 0, len(windowed))
		for _, row := range windowed {
			meter.Charge(Costs.RowProduced)
			projected := make([]Value, len(projections))
			for i, p := range projections {
				v, err := p.eval(row, bound, meter)
				if err != nil {
					return Outcome{}, err
				}
				projected[i] = v
			}
			out = append(out, projected)
		}
	}

	return Outcome{Kind: OutcomeQuery, ColumnNames: columnNames, Rows: out, Cost: meter.Accrued}, nil
}

// distinctRowKey encodes a projected row into a collision-free string key for DISTINCT
// dedup. Each field carries a type tag (n/i/b) and a payload, joined by a separator that
// no field can contain, so e.g. (1,23) and (12,3) do not collide (spec/design/grammar.md
// §11). NULL == NULL falls out (both encode to "n"), matching the NULL-safe DISTINCT rule.
func distinctRowKey(row []Value) string {
	var b strings.Builder
	for i, v := range row {
		if i > 0 {
			b.WriteByte('|')
		}
		switch v.Kind {
		case ValNull:
			b.WriteByte('n')
		case ValInt:
			b.WriteByte('i')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValBool:
			b.WriteByte('b')
			if v.Bool {
				b.WriteByte('1')
			} else {
				b.WriteByte('0')
			}
		case ValText:
			// Length-prefix the content so the separator byte cannot be confused with a
			// text value that contains it (the value bytes are arbitrary UTF-8).
			b.WriteByte('t')
			b.WriteString(strconv.Itoa(len(v.Str)))
			b.WriteByte(':')
			b.WriteString(v.Str)
		case ValDecimal:
			// Value-canonical key so 1.5 and 1.50 collapse to one DISTINCT bucket
			// (spec/design/decimal.md §5).
			b.WriteByte('d')
			b.WriteString(v.Dec.CanonicalString())
		case ValBytea:
			// Length-prefix the raw bytes (held in Str; a distinct 'y' tag, so a bytea never
			// collides with a text value of the same bytes).
			b.WriteByte('y')
			b.WriteString(strconv.Itoa(len(v.Str)))
			b.WriteByte(':')
			b.WriteString(v.Str)
		case ValUuid:
			// The 16 raw bytes (held in Str), under a distinct 'u' tag so a uuid never collides
			// with a bytea/text of the same bytes. Fixed-width, but length-prefixed for symmetry.
			b.WriteByte('u')
			b.WriteString(strconv.Itoa(len(v.Str)))
			b.WriteByte(':')
			b.WriteString(v.Str)
		}
	}
	return b.String()
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
	rtText        // text (one family, collation C); does not promote
	rtDecimal     // decimal (one family; the per-column typmod is carried separately)
	rtBytea       // bytea (one family, raw bytes); does not promote
	rtUuid        // uuid (one family, fixed 16 bytes); does not promote. First non-integer key.
	rtTimestamp   // timestamp (zoneless instant); does not compare/cast to timestamptz
	rtTimestamptz // timestamptz (UTC instant); does not compare/cast to timestamp
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

// ctxOf returns the type a sibling operand offers an adaptable operand. For an integer literal
// this is the integer width it adopts; for a string literal, bytea/uuid/text (so it can decode
// the hex/uuid input); a bind parameter additionally adopts a decimal/boolean sibling (a literal
// ignores those — its arm keeps int64/text — so widening the mapping is safe). Only a bare NULL
// offers no context (spec/design/api.md §5).
func ctxOf(t resolvedType) *ScalarType {
	switch t.kind {
	case rtInt:
		ty := t.intTy
		return &ty
	case rtBytea:
		ty := Bytea
		return &ty
	case rtUuid:
		ty := Uuid
		return &ty
	case rtText:
		ty := Text
		return &ty
	case rtBool:
		ty := Bool
		return &ty
	case rtDecimal:
		ty := DecimalType
		return &ty
	case rtTimestamp:
		ty := Timestamp
		return &ty
	case rtTimestamptz:
		ty := Timestamptz
		return &ty
	default:
		return nil
	}
}

// rExprKind tags a resolved expression node.
type rExprKind int

const (
	reColumn rExprKind = iota
	// reParam is a bind parameter, by 0-based index into the bound-values slice passed to eval.
	// Its static type was inferred from context at resolve (spec/design/api.md §5); the value is
	// supplied (and coerced) before evaluation.
	reParam
	reConstInt
	reConstBool
	reConstText
	reConstDecimal
	reConstBytea
	reConstUuid
	reConstTimestamp
	reConstTimestamptz
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
	reLike
	reCase
)

// rExpr is a resolved expression over fixed column indices, ready to evaluate against a
// row. Arithmetic/neg nodes carry their (promotion-tower) result type in `result` so the
// computed value can be range-checked against it.
type rExpr struct {
	kind    rExprKind
	index   int            // reColumn
	cInt    int64          // reConstInt
	cBool   bool           // reConstBool
	cText   string         // reConstText
	cDec    Decimal        // reConstDecimal
	cBytea  []byte         // reConstBytea
	op      BinaryOp       // reArith, reCompare
	result  ScalarType     // reCast target; reNeg / reArith result type
	typmod  *DecimalTypmod // reCast: a decimal target's numeric(p,s) typmod
	lhs     *rExpr         // reArith, reCompare, reAnd, reOr, reDistinct
	rhs     *rExpr         // reArith, reCompare, reAnd, reOr, reDistinct
	operand *rExpr         // reCast, reNeg, reNot, reIsNull
	negated bool           // reIsNull, reDistinct

	// reCase: (condition, result) arms, the ELSE result (constNull for an implicit ELSE), and
	// whether the unified result type is decimal (so integer results widen to decimal at eval).
	caseArms    []rCaseArm
	caseEls     *rExpr
	caseDecimal bool
}

// rCaseArm is one resolved (condition, result) branch of a reCase node (spec/design/grammar.md
// §23). The condition is the searched boolean predicate, or the simple form's resolved
// `operand = value` equality.
type rCaseArm struct {
	cond   *rExpr
	result *rExpr
}

// ============================================================================
// Aggregate resolution + accumulation (spec/design/aggregates.md).
//
// An aggregate query's select list resolves in "collect" mode: each aggregate call is
// collected into an aggSpec (its plan + resolved argument) and replaced by a reference to a
// synthetic-row slot (an reColumn indexing the finalized aggregate results), so the existing
// evaluator projects the result with no new node. Outside collect mode (WHERE / ON / an
// aggregate's own argument / any non-aggregate query) a column resolves normally and an
// aggregate call is a 42803 grouping error.
// ============================================================================

// aggCtx threads the aggregate-resolution mode through resolve. collecting == false is the
// Forbidden mode (a FuncCall is 42803; columns resolve normally); collecting == true is an
// aggregate query's projection (a FuncCall collects into specs and resolves to a synthetic
// slot len(groupKeys)+index; a column resolves to its position among groupKeys if it is a
// grouping key, else 42803). groupKeys holds the resolved flat indices of the GROUP BY
// columns (empty for whole-table aggregation). The synthetic row the projection evaluates
// against is [group_key_values..., agg_results...].
type aggCtx struct {
	collecting bool
	groupKeys  []int
	specs      []aggSpec
}

// aggPlan is the runtime plan for one aggregate, fixed at resolve from the function + operand
// type (the PG widening — spec/design/aggregates.md §3).
type aggPlan int

const (
	planCountStar  aggPlan = iota // COUNT(*) — count every row
	planCount                     // COUNT(expr) — count non-NULL inputs
	planSumInt                    // SUM(int16|int32) — accumulate i64, result int64 (trap at int64)
	planSumDecimal                // SUM(int64|decimal) — accumulate decimal, result decimal
	planAvg                       // AVG — decimal sum + i64 count; result sum/count (NULL if 0)
	planMin
	planMax
)

// aggSpec is one resolved aggregate: its plan and its resolved argument (evaluated per input
// row against the real row). operand is nil for COUNT(*).
type aggSpec struct {
	plan    aggPlan
	operand *rExpr
}

// acc is a running aggregate accumulator (one per aggSpec), folded per input row then finalized.
type acc struct {
	plan   aggPlan
	count  int64
	sumInt int64
	sumDec Decimal
	seen   bool
	cur    Value
	hasCur bool
}

func newAcc(plan aggPlan) *acc {
	a := &acc{plan: plan}
	if plan == planSumDecimal || plan == planAvg {
		a.sumDec = DecimalFromInt64(0)
	}
	return a
}

// fold folds one input value into the accumulator. NULL arguments are skipped (COUNT(*)
// ignores the value and always counts). Traps 22003 on SUM/AVG overflow at the result bound.
func (a *acc) fold(v Value) error {
	switch a.plan {
	case planCountStar:
		a.count++
	case planCount:
		if !v.IsNull() {
			a.count++
		}
	case planSumInt:
		if !v.IsNull() {
			s := a.sumInt + v.Int
			if (v.Int > 0 && s < a.sumInt) || (v.Int < 0 && s > a.sumInt) {
				return overflowErr(Int64)
			}
			a.sumInt = s
			a.seen = true
		}
	case planSumDecimal:
		if !v.IsNull() {
			d, err := a.sumDec.Add(toDecimal(v))
			if err != nil {
				return err
			}
			a.sumDec = d
			a.seen = true
		}
	case planAvg:
		if !v.IsNull() {
			d, err := a.sumDec.Add(toDecimal(v))
			if err != nil {
				return err
			}
			a.sumDec = d
			a.count++
		}
	case planMin, planMax:
		if !v.IsNull() {
			if !a.hasCur {
				a.cur, a.hasCur = v, true
			} else {
				c := valueCmp(a.cur, v)
				keepCur := (a.plan == planMin && c <= 0) || (a.plan == planMax && c >= 0)
				if !keepCur {
					a.cur = v
				}
			}
		}
	}
	return nil
}

// finalize produces the aggregate's final value over the group. COUNT → its count (0 over
// empty); SUM/MIN/MAX → NULL over an empty/all-NULL group; AVG → sum/count (NULL if count 0).
func (a *acc) finalize() (Value, error) {
	switch a.plan {
	case planCountStar, planCount:
		return IntValue(a.count), nil
	case planSumInt:
		if a.seen {
			return IntValue(a.sumInt), nil
		}
		return NullValue(), nil
	case planSumDecimal:
		if a.seen {
			return DecimalValue(a.sumDec), nil
		}
		return NullValue(), nil
	case planAvg:
		if a.count == 0 {
			return NullValue(), nil
		}
		d, err := a.sumDec.Div(DecimalFromInt64(a.count))
		if err != nil {
			return NullValue(), err
		}
		return DecimalValue(d), nil
	default: // planMin, planMax
		if a.hasCur {
			return a.cur, nil
		}
		return NullValue(), nil
	}
}

// itemsHaveAggregate reports whether any select item contains an aggregate call.
func itemsHaveAggregate(items SelectItems) bool {
	if items.All {
		return false
	}
	for _, it := range items.Items {
		if exprHasFuncCall(it.Expr) {
			return true
		}
	}
	return false
}

// exprHasFuncCall reports whether an expression tree contains a function (aggregate) call.
func exprHasFuncCall(e Expr) bool {
	switch e.Kind {
	case ExprFuncCall:
		return true
	case ExprCast:
		return exprHasFuncCall(e.Cast.Inner)
	case ExprUnary:
		return exprHasFuncCall(e.Unary.Operand)
	case ExprIsNull:
		return exprHasFuncCall(e.IsNullOf.Operand)
	case ExprBinary:
		return exprHasFuncCall(e.Binary.Lhs) || exprHasFuncCall(e.Binary.Rhs)
	case ExprIsDistinct:
		return exprHasFuncCall(e.IsDistinct.Lhs) || exprHasFuncCall(e.IsDistinct.Rhs)
	case ExprIn:
		if exprHasFuncCall(e.In.Lhs) {
			return true
		}
		for _, elem := range e.In.List {
			if exprHasFuncCall(elem) {
				return true
			}
		}
		return false
	case ExprBetween:
		return exprHasFuncCall(e.Between.Lhs) || exprHasFuncCall(e.Between.Lo) || exprHasFuncCall(e.Between.Hi)
	case ExprLike:
		return exprHasFuncCall(e.Like.Lhs) || exprHasFuncCall(e.Like.Rhs)
	case ExprCase:
		if e.Case.Operand != nil && exprHasFuncCall(*e.Case.Operand) {
			return true
		}
		for _, w := range e.Case.Whens {
			if exprHasFuncCall(w.Cond) || exprHasFuncCall(w.Result) {
				return true
			}
		}
		return e.Case.Els != nil && exprHasFuncCall(*e.Case.Els)
	default:
		return false
	}
}

// resolveAggregate resolves an aggregate call into a synthetic-row reference, collecting its
// aggSpec. Valid only in collect mode; in Forbidden mode (WHERE/ON/nested) it is 42803. The
// operand resolves in a fresh Forbidden sub-context (a nested aggregate is 42803; its columns
// resolve against the real row). The result type follows the PG widening (aggregates.md §3).
func resolveAggregate(s *scope, fc *FuncCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if !ag.collecting {
		return nil, resolvedType{}, NewError(GroupingError, "aggregate functions are not allowed here")
	}
	name := toLowerASCII(fc.Name)
	sub := &aggCtx{collecting: false}
	var (
		plan    aggPlan
		operand *rExpr
		result  resolvedType
	)
	switch name {
	case "count":
		if fc.Star {
			plan, operand, result = planCountStar, nil, resolvedType{kind: rtInt, intTy: Int64}
		} else {
			r, _, err := resolve(s, *fc.Arg, nil, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			plan, operand, result = planCount, r, resolvedType{kind: rtInt, intTy: Int64}
		}
	case "sum", "avg", "min", "max":
		if fc.Star {
			return nil, resolvedType{}, NewError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		r, t, err := resolve(s, *fc.Arg, nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch name {
		case "sum":
			switch {
			case t.kind == rtInt && t.intTy == Int64:
				plan, operand, result = planSumDecimal, r, resolvedType{kind: rtDecimal}
			case t.kind == rtInt:
				plan, operand, result = planSumInt, r, resolvedType{kind: rtInt, intTy: Int64}
			case t.kind == rtDecimal:
				plan, operand, result = planSumDecimal, r, resolvedType{kind: rtDecimal}
			default:
				return nil, resolvedType{}, noAggOverload("sum")
			}
		case "avg":
			if t.kind == rtInt || t.kind == rtDecimal {
				plan, operand, result = planAvg, r, resolvedType{kind: rtDecimal}
			} else {
				return nil, resolvedType{}, noAggOverload("avg")
			}
		case "min":
			plan, operand, result = planMin, r, t
		case "max":
			plan, operand, result = planMax, r, t
		}
	default:
		return nil, resolvedType{}, NewError(UndefinedFunction, "function does not exist: "+fc.Name)
	}
	// Aggregate results follow the group-key values in the synthetic row.
	slot := len(ag.groupKeys) + len(ag.specs)
	ag.specs = append(ag.specs, aggSpec{plan: plan, operand: operand})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// noAggOverload is 42883 — an aggregate over an operand family it has no overload for.
func noAggOverload(fn string) error {
	return NewError(UndefinedFunction, "no "+fn+" aggregate for that argument type")
}

// groupingErrorColumn is the 42803 for a non-aggregated column not in GROUP BY.
func groupingErrorColumn(name string) error {
	return NewError(GroupingError, "column "+name+" must appear in the GROUP BY clause or be used in an aggregate function")
}

// collectColumn resolves a column reference (already at real flat index idx) under an
// aggregate context. In Forbidden mode it reads the real row directly; in collect mode it must
// be a grouping key — resolved to its synthetic-row slot (its position among the group keys) —
// else 42803.
func collectColumn(s *scope, ag *aggCtx, idx int, name string) (*rExpr, resolvedType, error) {
	ty := resolvedTypeOf(s.columnAt(idx).Type)
	if !ag.collecting {
		return &rExpr{kind: reColumn, index: idx}, ty, nil
	}
	for pos, gk := range ag.groupKeys {
		if gk == idx {
			return &rExpr{kind: reColumn, index: pos}, ty, nil
		}
	}
	return nil, resolvedType{}, groupingErrorColumn(name)
}

// ============================================================================
// Resolution scope (multi-table FROM — spec/design/grammar.md §15).
//
// A scope is the ordered list of relations a SELECT's FROM clause puts in scope, each
// carrying the flat COLUMN OFFSET at which its columns begin in the concatenated (joined)
// row. A resolved column reference bakes a single flat index offset+local into reColumn, so
// the joined row is just each relation's row concatenated in FROM order and the evaluator is
// unchanged. A single-table SELECT / UPDATE / DELETE is a one-relation scope (offset 0).
//
// NOTE (forward-compat): the scope keys resolution ONLY on column name and type — never on a
// column's NotNull / PrimaryKey flags. A column on the nullable side of a future outer join
// is NULL-extended at runtime regardless of its declared nullability (grammar.md §15).
// ============================================================================

// scopeRel is one relation in a FROM scope: its label (alias, else table name, lower-cased
// for case-insensitive matching), the table, and the flat offset of its first column.
type scopeRel struct {
	label  string
	table  *Table
	offset int
}

// scope is the relations a query's FROM clause puts in scope, in FROM order.
type scope struct {
	rels []scopeRel
}

// singleScope is a one-relation scope (the single-table SELECT / UPDATE / DELETE case).
func singleScope(t *Table) *scope {
	return &scope{rels: []scopeRel{{label: strings.ToLower(t.Name), table: t, offset: 0}}}
}

// resolveBare resolves a bare column name to a flat row index: no relation has it → 42703;
// two or more relations have it → 42702 ambiguous; exactly one → its flat index.
func (s *scope) resolveBare(name string) (int, error) {
	found := -1
	for _, r := range s.rels {
		if local := r.table.ColumnIndex(name); local >= 0 {
			if found >= 0 {
				return 0, ambiguousColumn(name)
			}
			found = r.offset + local
		}
	}
	if found < 0 {
		return 0, undefinedColumn(name)
	}
	return found, nil
}

// resolveQualified resolves a qualified rel.col to a flat row index: an unknown rel is 42P01,
// a known rel with no such column is 42703. Never ambiguous (it names one relation).
func (s *scope) resolveQualified(qualifier, name string) (int, error) {
	q := strings.ToLower(qualifier)
	for _, r := range s.rels {
		if r.label == q {
			local := r.table.ColumnIndex(name)
			if local < 0 {
				return 0, undefinedColumn(name)
			}
			return r.offset + local, nil
		}
	}
	return 0, missingFromEntry(qualifier)
}

// columnAt returns the column at a flat index (the index is known valid — resolution made it).
func (s *scope) columnAt(flat int) *Column {
	for i := range s.rels {
		r := s.rels[i]
		n := len(r.table.Columns)
		if flat >= r.offset && flat < r.offset+n {
			return &r.table.Columns[flat-r.offset]
		}
	}
	panic("a resolved flat column index is always in range")
}

// undefinedColumn is 42703 — a column name that no relation in scope defines.
func undefinedColumn(name string) error {
	return NewError(UndefinedColumn, "column does not exist: "+name)
}

// ambiguousColumn is 42702 — a bare column name that more than one relation in scope defines.
func ambiguousColumn(name string) error {
	return NewError(AmbiguousColumn, "column reference "+name+" is ambiguous")
}

// missingFromEntry is 42P01 — a qualifier that names no relation in the FROM clause.
func missingFromEntry(qualifier string) error {
	return NewError(UndefinedTable, "missing FROM-clause entry for table "+qualifier)
}

// paramTypes accumulates the inferred type of each bind parameter ($N) across every clause of a
// statement (spec/design/api.md §5). types[i] is the inferred scalar type of $(i+1); a nil entry
// marks a parameter referenced before any context fixed its type.
type paramTypes struct {
	types []*ScalarType
}

// note records that $(idx0+1) appears with context type ty (nil = no context here). It unifies
// with any prior inference: equal types agree, two integer widths widen to the wider, an
// incompatible concrete pair is 42804.
func (p *paramTypes) note(idx0 int, ty *ScalarType) error {
	for idx0 >= len(p.types) {
		p.types = append(p.types, nil)
	}
	if ty == nil {
		return nil
	}
	if p.types[idx0] == nil {
		t := *ty
		p.types[idx0] = &t
		return nil
	}
	u, err := unifyParamType(*p.types[idx0], *ty, idx0)
	if err != nil {
		return err
	}
	p.types[idx0] = &u
	return nil
}

// finalize returns the ordered parameter types. A slot referenced but never typed — including a
// gap in $1..$N — is 42P18 indeterminate_datatype.
func (p *paramTypes) finalize() ([]ScalarType, error) {
	out := make([]ScalarType, 0, len(p.types))
	for i, t := range p.types {
		if t == nil {
			return nil, NewError(IndeterminateDatatype,
				fmt.Sprintf("could not determine data type of parameter $%d", i+1))
		}
		out = append(out, *t)
	}
	return out, nil
}

// unifyParamType unifies two inferred types for the same parameter: equal agrees; two integer
// widths widen to the wider; any other mismatch is 42804 (spec/design/api.md §5).
func unifyParamType(a, b ScalarType, idx0 int) (ScalarType, error) {
	if a == b {
		return a, nil
	}
	if a.IsInteger() && b.IsInteger() {
		if a.Rank() >= b.Rank() {
			return a, nil
		}
		return b, nil
	}
	var zero ScalarType
	return zero, NewError(DatatypeMismatch,
		fmt.Sprintf("inconsistent types inferred for parameter $%d", idx0+1))
}

// bindParams coerces each supplied bind value to its inferred parameter type, two-phase /
// all-or-nothing like INSERT (spec/design/api.md §5): a count mismatch is 42601 and every value
// is validated up front (22003/42804/22P02/23502 via storeValue) before any row is touched.
func bindParams(supplied []Value, types []ScalarType) ([]Value, error) {
	if len(supplied) != len(types) {
		return nil, NewError(SyntaxError, fmt.Sprintf(
			"bind parameter count mismatch: statement expects %d, got %d", len(types), len(supplied),
		))
	}
	bound := make([]Value, len(types))
	for i, ty := range types {
		v, err := storeValue(supplied[i], ty, nil, false, fmt.Sprintf("$%d", i+1))
		if err != nil {
			return nil, err
		}
		bound[i] = v
	}
	return bound, nil
}

// resolvedTypeOf is the resolved (static) type of a column of scalar type ty.
func resolvedTypeOf(ty ScalarType) resolvedType {
	switch {
	case ty.IsText():
		return resolvedType{kind: rtText}
	case ty.IsBool():
		return resolvedType{kind: rtBool}
	case ty.IsDecimal():
		return resolvedType{kind: rtDecimal}
	case ty.IsBytea():
		return resolvedType{kind: rtBytea}
	case ty.IsUuid():
		return resolvedType{kind: rtUuid}
	case ty.IsTimestamp():
		return resolvedType{kind: rtTimestamp}
	case ty.IsTimestamptz():
		return resolvedType{kind: rtTimestamptz}
	default:
		return resolvedType{kind: rtInt, intTy: ty}
	}
}

// resolveProjections resolves SELECT items into evaluable projections (any result type is
// allowed in the select list, including boolean — SELECT a = b), each paired with its output
// column name (spec/design/grammar.md §8). `*` expands across ALL relations in FROM order,
// each relation's columns in catalog order (§15).
func resolveProjections(s *scope, items SelectItems, ag *aggCtx, params *paramTypes) ([]*rExpr, []string, error) {
	if items.All {
		var ps []*rExpr
		var names []string
		for _, r := range s.rels {
			for i := range r.table.Columns {
				ps = append(ps, &rExpr{kind: reColumn, index: r.offset + i})
				names = append(names, r.table.Columns[i].Name)
			}
		}
		return ps, names, nil
	}
	ps := make([]*rExpr, 0, len(items.Items))
	names := make([]string, 0, len(items.Items))
	for _, it := range items.Items {
		node, _, err := resolve(s, it.Expr, nil, ag, params)
		if err != nil {
			return nil, nil, err
		}
		ps = append(ps, node)
		if it.Alias != nil {
			names = append(names, *it.Alias)
		} else {
			names = append(names, outputName(s, it.Expr))
		}
	}
	return ps, names, nil
}

// outputName is the output column name of an un-aliased select item (grammar.md §8/§15): a
// bare or qualified column reference takes the catalog's canonical name (never the qualifier,
// never the SELECT spelling); every other expression takes the fixed "?column?". The column
// is known to exist — resolve validated it.
func outputName(s *scope, e Expr) string {
	switch e.Kind {
	case ExprColumn:
		if idx, err := s.resolveBare(e.Column); err == nil {
			return s.columnAt(idx).Name
		}
		return e.Column
	case ExprQualifiedColumn:
		if idx, err := s.resolveQualified(e.Qualifier, e.Column); err == nil {
			return s.columnAt(idx).Name
		}
		return e.Column
	case ExprFuncCall:
		// An un-aliased aggregate call is named by its lowercased function name (PG; §8).
		return toLowerASCII(e.FuncCall.Name)
	default:
		return "?column?"
	}
}

// resolveBooleanFilter resolves a WHERE / ON expression; it must resolve to boolean (or an
// untyped NULL, which is always unknown → no rows). An integer- or text-valued one is 42804.
func resolveBooleanFilter(s *scope, e *Expr, params *paramTypes) (*rExpr, error) {
	// WHERE / ON filters run before any grouping, so an aggregate here is 42803 (Forbidden).
	node, ty, err := resolve(s, *e, nil, &aggCtx{collecting: false}, params)
	if err != nil {
		return nil, err
	}
	if ty.kind != rtBool && ty.kind != rtNull {
		return nil, typeError("argument of WHERE must be boolean")
	}
	return node, nil
}

// resolve resolves one Expr into an rExpr plus its static type. ctx (non-nil) is the
// type an untyped integer literal should adapt to (spec/design/types.md §6); nil
// defaults a bare literal to int64.
func resolve(s *scope, e Expr, ctx *ScalarType, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	switch e.Kind {
	case ExprParam:
		// A bind parameter is an adaptable operand (like an integer/string literal): it takes its
		// type from ctx — the sibling operand, target column, or CAST target. Record the inferred
		// type (nil = no context here; finalize 42P18s a parameter that never gets one).
		idx0 := int(e.Param) - 1
		if err := params.note(idx0, ctx); err != nil {
			return nil, resolvedType{}, err
		}
		var rty resolvedType
		if ctx != nil {
			rty = resolvedTypeOf(*ctx)
		} else {
			rty = resolvedType{kind: rtNull}
		}
		return &rExpr{kind: reParam, index: idx0}, rty, nil
	case ExprColumn:
		// Resolve for existence first (42703/42702 take priority, matching PostgreSQL); then
		// in an aggregate query's projection the column must be a grouping key (else 42803).
		idx, err := s.resolveBare(e.Column)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return collectColumn(s, ag, idx, e.Column)
	case ExprQualifiedColumn:
		idx, err := s.resolveQualified(e.Qualifier, e.Column)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return collectColumn(s, ag, idx, e.Column)
	case ExprFuncCall:
		return resolveAggregate(s, e.FuncCall, ag, params)
	case ExprLiteral:
		switch e.Literal.Kind {
		case LiteralNull:
			return &rExpr{kind: reConstNull}, resolvedType{kind: rtNull}, nil
		case LiteralBool:
			return &rExpr{kind: reConstBool, cBool: e.Literal.Bool}, resolvedType{kind: rtBool}, nil
		case LiteralText:
			// A string literal is text by default (collation C). It adapts to a BYTEA or a UUID
			// context (types.md §6/§13/§14): decode the hex input (bytea) or the PG-flexible uuid
			// input (uuid) — 22P02 on malformed; any other context — including none — keeps it text.
			// A string literal is text by default (collation C). It adapts to a BYTEA context (hex
			// input, 22P02), a UUID context (PG-flexible input, 22P02 — types.md §6/§13/§14), or a
			// TIMESTAMP/TIMESTAMPTZ context (parse the datetime, 22007/22008 — spec/design/timestamp.md).
			switch {
			case ctx != nil && ctx.IsBytea():
				b, err := decodeByteaLiteral(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstBytea, cBytea: b}, resolvedType{kind: rtBytea}, nil
			case ctx != nil && ctx.IsUuid():
				b, err := decodeUUIDLiteral(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstUuid, cBytea: b}, resolvedType{kind: rtUuid}, nil
			case ctx != nil && ctx.IsTimestamp():
				m, err := ParseTimestamp(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstTimestamp, cInt: m}, resolvedType{kind: rtTimestamp}, nil
			case ctx != nil && ctx.IsTimestamptz():
				m, err := ParseTimestamptz(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstTimestamptz, cInt: m}, resolvedType{kind: rtTimestamptz}, nil
			}
			return &rExpr{kind: reConstText, cText: e.Literal.Str}, resolvedType{kind: rtText}, nil
		case LiteralDecimal:
			// A decimal literal is always decimal; it does not adapt to context (like text).
			// Cap-check it here (an over-long coefficient/scale traps 22003 at resolve).
			d, err := e.Literal.Dec.CheckCap()
			if err != nil {
				return nil, resolvedType{}, err
			}
			return &rExpr{kind: reConstDecimal, cDec: d}, resolvedType{kind: rtDecimal}, nil
		default: // LiteralInt
			// An integer literal adapts only to an integer context; a non-integer context
			// (a text/decimal column or assignment target) does not apply — it defaults to
			// int64, and the surrounding check then reports the family mismatch (42804) or
			// widens it (int→decimal), never a wrong range check on a non-integer type.
			ty := Int64
			if ctx != nil && ctx.IsInteger() {
				ty = *ctx
			}
			if !ty.InRange(e.Literal.Int) {
				return nil, resolvedType{}, overflowErr(ty)
			}
			return &rExpr{kind: reConstInt, cInt: e.Literal.Int},
				resolvedType{kind: rtInt, intTy: ty}, nil
		}
	case ExprCast:
		target, typmod, err := resolveTypeAndTypmod(e.Cast.TypeName, e.Cast.TypeMod)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// Text casts are deferred (not in the cast matrix — spec/design/types.md §5/§11):
		// casting TO text is a 0A000 this slice.
		if target.IsText() {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting to text is not supported yet")
		}
		// Boolean casts are likewise deferred (boolean⇄integer is a later cast slice —
		// spec/types/casts.toml): casting TO boolean is a 0A000 this slice. Without this
		// guard resolveTypeAndTypmod now returns boolean, so it must be caught here.
		if target.IsBool() {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting to boolean is not supported yet")
		}
		// bytea casts are likewise deferred (types.md §5/§13): casting TO bytea is 0A000.
		if target.IsBytea() {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting to bytea is not supported yet")
		}
		// uuid casts are likewise deferred (types.md §5/§14): casting TO uuid is 0A000.
		if target.IsUuid() {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting to uuid is not supported yet")
		}
		// timestamp casts are deferred (spec/design/timestamp.md §6): casting TO a datetime is 0A000.
		if target.IsTimestamp() || target.IsTimestamptz() {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting to a timestamp type is not supported yet")
		}
		inner, ity, err := resolve(s, e.Cast.Inner, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ity.kind == rtBool {
			return nil, resolvedType{}, typeError("cannot cast boolean to " + target.CanonicalName())
		}
		// Casting FROM text is likewise deferred (0A000).
		if ity.kind == rtText {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting from text is not supported yet")
		}
		// Casting FROM bytea is likewise deferred (0A000).
		if ity.kind == rtBytea {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting from bytea is not supported yet")
		}
		// Casting FROM uuid is likewise deferred (0A000).
		if ity.kind == rtUuid {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting from uuid is not supported yet")
		}
		// Casting FROM a timestamp is likewise deferred (0A000).
		if ity.kind == rtTimestamp || ity.kind == rtTimestamptz {
			return nil, resolvedType{}, NewError(FeatureNotSupported, "casting from a timestamp type is not supported yet")
		}
		// int→int (range check), int→decimal (widen), decimal→int (explicit, round),
		// decimal→decimal (re-scale), and NULL are all castable.
		resultRt := resolvedType{kind: rtInt, intTy: target}
		if target.IsDecimal() {
			resultRt = resolvedType{kind: rtDecimal}
		}
		return &rExpr{kind: reCast, operand: inner, result: target, typmod: typmod}, resultRt, nil
	case ExprUnary:
		if e.Unary.Op == OpNeg {
			rop, ty, err := resolve(s, e.Unary.Operand, ctx, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			switch ty.kind {
			case rtInt:
				return &rExpr{kind: reNeg, operand: rop, result: ty.intTy},
					resolvedType{kind: rtInt, intTy: ty.intTy}, nil
			case rtDecimal:
				return &rExpr{kind: reNeg, operand: rop, result: DecimalType},
					resolvedType{kind: rtDecimal}, nil
			case rtNull:
				return &rExpr{kind: reNeg, operand: rop, result: Int64}, // -NULL = NULL
					resolvedType{kind: rtInt, intTy: Int64}, nil
			default: // rtBool, rtText
				return nil, resolvedType{}, typeError("unary minus requires a numeric operand")
			}
		}
		// OpNot
		rop, ty, err := resolve(s, e.Unary.Operand, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireBool(ty, "NOT requires a boolean operand"); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reNot, operand: rop}, resolvedType{kind: rtBool}, nil
	case ExprIsNull:
		rop, _, err := resolve(s, e.IsNullOf.Operand, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reIsNull, operand: rop, negated: e.IsNullOf.Negated},
			resolvedType{kind: rtBool}, nil
	case ExprIsDistinct:
		// NULL-safe equality: the SAME operand contract as `=` — resolve the pair (a
		// literal adapts to its sibling; a text literal stays text), then require the
		// operands be comparable (both integer-ish or both text-ish; a mixed pair is
		// 42804). The result is always a definite boolean (functions.md §3).
		rl, lt, rr, rt, err := resolveOperandPair(s, e.IsDistinct.Lhs, e.IsDistinct.Rhs, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := classifyComparable(lt, rt); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reDistinct, lhs: rl, rhs: rr, negated: e.IsDistinct.Negated},
			resolvedType{kind: rtBool}, nil
	case ExprIn:
		// Desugar to the OR-chain PostgreSQL DEFINES `IN` as: `x IN (a,b,c)` is
		// `x = a OR x = b OR x = c`; `NOT IN` is its negation (grammar.md §20). The list is
		// non-empty (the parser rejects `IN ()` → 42601). Resolving the desugared tree reuses
		// the `=`/OR/NOT machinery verbatim, so the three-valued NULL semantics, per-element
		// operand typing (a too-wide literal → 22003, a cross-family element → 42804), and cost
		// all fall out. The LHS is evaluated once per element (the OR-chain model — a documented
		// cost consequence, cost.md §3).
		var folded Expr
		for i, elem := range e.In.List {
			eq := binaryExpr(OpEq, e.In.Lhs, elem)
			if i == 0 {
				folded = eq
			} else {
				folded = binaryExpr(OpOr, folded, eq)
			}
		}
		if e.In.Negated {
			folded = Expr{Kind: ExprUnary, Unary: &UnaryExpr{Op: OpNot, Operand: folded}}
		}
		return resolve(s, folded, ctx, ag, params)
	case ExprBetween:
		// Desugar to `lhs >= lo AND lhs <= hi` (grammar.md §21). The Kleene AND gives the PG
		// result for a NULL bound: `5 BETWEEN 10 AND NULL` is `FALSE AND NULL` = FALSE (a FALSE
		// operand dominates), while `5 BETWEEN 1 AND NULL` is `TRUE AND NULL` = NULL. NOT BETWEEN
		// negates the whole conjunction. The LHS is evaluated twice (the desugar model — a
		// documented cost consequence, cost.md §3).
		ge := binaryExpr(OpGe, e.Between.Lhs, e.Between.Lo)
		le := binaryExpr(OpLe, e.Between.Lhs, e.Between.Hi)
		folded := binaryExpr(OpAnd, ge, le)
		if e.Between.Negated {
			folded = Expr{Kind: ExprUnary, Unary: &UnaryExpr{Op: OpNot, Operand: folded}}
		}
		return resolve(s, folded, ctx, ag, params)
	case ExprLike:
		// LIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal stays
		// text), then require BOTH operands be text (or a bare NULL); a non-text operand is
		// 42804. We do NOT use classifyComparable here — it would wrongly accept bytea×bytea.
		rl, lt, rr, rt, err := resolveOperandPair(s, e.Like.Lhs, e.Like.Rhs, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireTextOrNull(lt); err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireTextOrNull(rt); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reLike, lhs: rl, rhs: rr, negated: e.Like.Negated},
			resolvedType{kind: rtBool}, nil
	case ExprCase:
		// Resolve each branch's condition: searched form requires a boolean WHEN (42804
		// otherwise); simple form desugars to `operand = value` (reusing the `=` operand pairing
		// + comparability check, so the value adapts to the operand's type). The operand is
		// evaluated once per tested branch (the desugar model, like IN).
		arms := make([]rCaseArm, 0, len(e.Case.Whens))
		resultTypes := make([]resolvedType, 0, len(e.Case.Whens)+1)
		for _, w := range e.Case.Whens {
			var rcond *rExpr
			if e.Case.Operand != nil {
				eq := binaryExpr(OpEq, *e.Case.Operand, w.Cond)
				rc, _, err := resolve(s, eq, nil, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				rcond = rc
			} else {
				rc, cty, err := resolve(s, w.Cond, nil, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				if err := requireBool(cty, "CASE WHEN condition must be boolean"); err != nil {
					return nil, resolvedType{}, err
				}
				rcond = rc
			}
			rres, rty, err := resolve(s, w.Result, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			resultTypes = append(resultTypes, rty)
			arms = append(arms, rCaseArm{cond: rcond, result: rres})
		}
		var rels *rExpr
		if e.Case.Els != nil {
			r, ety, err := resolve(s, *e.Case.Els, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			rels = r
			resultTypes = append(resultTypes, ety)
		} else {
			rels = &rExpr{kind: reConstNull}
			resultTypes = append(resultTypes, resolvedType{kind: rtNull})
		}
		unified, err := unifyCaseTypes(resultTypes)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reCase, caseArms: arms, caseEls: rels, caseDecimal: unified.kind == rtDecimal},
			unified, nil
	default: // ExprBinary
		return resolveBinary(s, e.Binary, ag, params)
	}
}

func resolveBinary(s *scope, b *BinaryExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	switch b.Op {
	case OpAdd, OpSub, OpMul, OpDiv, OpMod:
		// Arithmetic is overloaded across integer and decimal. Resolve the operand pair (an
		// integer literal adapts to an integer sibling), then pick the family: both integer →
		// integer arithmetic; at least one decimal → decimal arithmetic (the integer operand
		// widens at eval); a text/boolean operand is a 42804 (spec/design/decimal.md §4).
		rl, lt, rr, rt, err := resolveOperandPair(s, b.Lhs, b.Rhs, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireNumericOperand(lt); err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireNumericOperand(rt); err != nil {
			return nil, resolvedType{}, err
		}
		if lt.kind == rtDecimal || rt.kind == rtDecimal {
			return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: DecimalType},
				resolvedType{kind: rtDecimal}, nil
		}
		result := promote(lt, rt)
		return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: result},
			resolvedType{kind: rtInt, intTy: result}, nil
	case OpEq, OpLt, OpGt, OpLe, OpGe:
		// Comparison is overloaded across families: integer×integer or text×text. Resolve
		// the operands (a literal adapts to its sibling; text literals stay text), then
		// require they be comparable — a mixed integer/text pair is 42804. The runtime
		// comparison (Eq3/Lt3/Gt3) dispatches on the value kinds.
		rl, lt, rr, rt, err := resolveOperandPair(s, b.Lhs, b.Rhs, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := classifyComparable(lt, rt); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reCompare, op: b.Op, lhs: rl, rhs: rr},
			resolvedType{kind: rtBool}, nil
	default: // OpAnd, OpOr
		rl, lt, err := resolve(s, b.Lhs, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rr, rt, err := resolve(s, b.Rhs, nil, ag, params)
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

// resolveOperandPair resolves the two operands of a binary operator, giving a bare
// *integer* literal the other operand's integer type as context (so `small + 1` types `1`
// as int16, and `small + 100000` traps 22003 at resolve). A text literal needs no context
// (it is always text); when the sibling is text, an integer literal gets no integer
// context (ctxOf returns nil) and defaults to int64 — the caller's family check then
// reports the mismatch. This does NOT enforce a family — resolveIntPair (arithmetic) and
// classifyComparable (comparison) layer that on top.
func resolveOperandPair(s *scope, lhs, rhs Expr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, *rExpr, resolvedType, error) {
	lhsLit := isAdaptableOperand(lhs)
	rhsLit := isAdaptableOperand(rhs)
	var rl, rr *rExpr
	var lt, rt resolvedType
	var err error
	switch {
	case lhsLit && rhsLit:
		i64 := Int64
		if rl, lt, err = resolve(s, lhs, &i64, ag, params); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(s, rhs, &i64, ag, params)
	case lhsLit:
		if rr, rt, err = resolve(s, rhs, nil, ag, params); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rl, lt, err = resolve(s, lhs, ctxOf(rt), ag, params)
	case rhsLit:
		if rl, lt, err = resolve(s, lhs, nil, ag, params); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(s, rhs, ctxOf(lt), ag, params)
	default:
		if rl, lt, err = resolve(s, lhs, nil, ag, params); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(s, rhs, nil, ag, params)
	}
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	return rl, lt, rr, rt, nil
}

// requireNumericOperand requires that an arithmetic operand is numeric (integer or decimal,
// or NULL); a boolean or text operand is a 42804 type error.
func requireNumericOperand(t resolvedType) error {
	if t.kind == rtBool || t.kind == rtText || t.kind == rtBytea || t.kind == rtUuid ||
		t.kind == rtTimestamp || t.kind == rtTimestamptz {
		return typeError("arithmetic operators require numeric operands")
	}
	return nil
}

// classifyComparable requires that a comparison operand pair is comparable
// (spec/types/compare.toml): both numeric (integer and/or decimal — the integer promotes to
// decimal), both text, or both boolean (NULL counts as either). A mixed numeric/text pair, or
// a boolean with a non-boolean, is a 42804 type error — comparison is overloaded across these
// families but never compares across them.
func classifyComparable(lt, rt resolvedType) error {
	// Boolean compares only with boolean (or NULL); boolean with a number/text is a mismatch.
	boolL, boolR := lt.kind == rtBool, rt.kind == rtBool
	if boolL != boolR && (lt.kind != rtNull && rt.kind != rtNull) {
		return typeError("cannot compare a boolean value with a non-boolean value")
	}
	lNum := lt.kind == rtInt || lt.kind == rtDecimal
	rNum := rt.kind == rtInt || rt.kind == rtDecimal
	if (lNum && rt.kind == rtText) || (lt.kind == rtText && rNum) {
		return typeError("cannot compare a text value with a numeric value")
	}
	// bytea compares only with bytea (or NULL); bytea with a number or text is a mismatch.
	byteaL, byteaR := lt.kind == rtBytea, rt.kind == rtBytea
	if byteaL != byteaR && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare a bytea value with a non-bytea value")
	}
	// uuid compares only with uuid (or NULL); uuid with anything else is a mismatch.
	uuidL, uuidR := lt.kind == rtUuid, rt.kind == rtUuid
	if uuidL != uuidR && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare a uuid value with a non-uuid value")
	}
	// timestamp / timestamptz compare only within their own family (or with NULL). A mixed
	// timestamp × timestamptz pair, or a datetime vs any other family, would need a zone, so
	// it is a 42804 type error (spec/design/timestamp.md §5).
	tsL := lt.kind == rtTimestamp || lt.kind == rtTimestamptz
	tsR := rt.kind == rtTimestamp || rt.kind == rtTimestamptz
	if (tsL || tsR) && lt.kind != rt.kind && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare a timestamp value with a value of a different type")
	}
	return nil
}

// isAdaptableOperand reports whether e is an adaptable operand — one that takes its type from
// its sibling: an integer or string literal, or a bind parameter $N (spec/design/api.md §5).
// NULL, boolean, and decimal literals do not take a sibling's context here.
func isAdaptableOperand(e Expr) bool {
	if e.Kind == ExprParam {
		return true
	}
	return e.Kind == ExprLiteral && (e.Literal.Kind == LiteralInt || e.Literal.Kind == LiteralText)
}

// decodeByteaLiteral decodes a single-quoted literal's content as a bytea value via the hex
// input form (ParseByteaHex), mapping malformed hex to a 22P02 (invalid_text_representation).
// Used when a string literal adapts to a bytea context (types.md §6/§13); the trap is
// deterministic and fires at resolve time, before any scan.
func decodeByteaLiteral(s string) ([]byte, error) {
	b, reason := ParseByteaHex(s)
	if reason != "" {
		return nil, NewError(InvalidTextRepresentation, "invalid input syntax for type bytea: "+reason)
	}
	return b, nil
}

// decodeUUIDLiteral decodes a single-quoted literal's content as a uuid value via the
// PG-flexible input (ParseUUID), mapping malformed input to a 22P02. Used when a string literal
// adapts to a uuid context (types.md §6/§14); deterministic, fires at resolve time before any scan.
func decodeUUIDLiteral(s string) ([]byte, error) {
	b, reason := ParseUUID(s)
	if reason != "" {
		return nil, NewError(InvalidTextRepresentation, "invalid input syntax for type uuid: "+reason)
	}
	return b, nil
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

func requireBool(t resolvedType, msg string) error {
	if t.kind == rtInt || t.kind == rtText || t.kind == rtDecimal || t.kind == rtBytea || t.kind == rtUuid ||
		t.kind == rtTimestamp || t.kind == rtTimestamptz {
		return typeError(msg)
	}
	return nil
}

// requireTextOrNull: LIKE requires both operands be text (or a bare NULL literal, which is
// comparable with anything and makes the result NULL at eval). A non-text operand is a 42804
// type error (spec/design/grammar.md §22).
func requireTextOrNull(t resolvedType) error {
	if t.kind == rtText || t.kind == rtNull {
		return nil
	}
	return typeError("LIKE requires text operands")
}

// unifyCaseTypes unifies a CASE's result-arm types (the THEN results + the ELSE, or rtNull for an
// implicit ELSE) into one common type (spec/design/grammar.md §23): NULL-typed arms are dropped
// (they adapt); an all-NULL CASE is text (PostgreSQL). The non-NULL arms must share a family — all
// numeric unify to decimal if any is decimal, else the widest integer (the promotion tower);
// otherwise they must all be the same non-numeric family (text/boolean/bytea). A cross-family mix
// is 42804.
func unifyCaseTypes(arms []resolvedType) (resolvedType, error) {
	nonNull := make([]resolvedType, 0, len(arms))
	for _, t := range arms {
		if t.kind != rtNull {
			nonNull = append(nonNull, t)
		}
	}
	if len(nonNull) == 0 {
		// Every arm is NULL/untyped — PostgreSQL types the CASE as text.
		return resolvedType{kind: rtText}, nil
	}
	allNumeric, anyDecimal := true, false
	for _, t := range nonNull {
		if t.kind != rtInt && t.kind != rtDecimal {
			allNumeric = false
		}
		if t.kind == rtDecimal {
			anyDecimal = true
		}
	}
	if allNumeric {
		if anyDecimal {
			return resolvedType{kind: rtDecimal}, nil
		}
		// All integer: the widest via the promotion tower (width is unobservable in output —
		// every integer renders under the `I` tag — but the fold keeps the type precise).
		acc := nonNull[0]
		for _, t := range nonNull[1:] {
			acc = resolvedType{kind: rtInt, intTy: promote(acc, t)}
		}
		return acc, nil
	}
	// Non-numeric: every arm must be the same family as the first (cross-family is 42804).
	first := nonNull[0]
	for _, t := range nonNull[1:] {
		if t.kind != first.kind {
			return resolvedType{}, typeError("CASE result types must be compatible")
		}
	}
	return first, nil
}

// coerceCase coerces a CASE arm's value to the unified result type. The only runtime coercion
// needed is widening an integer result to decimal when the unified type is decimal — integer-width
// unification needs none (all integers are int64), and an all-NULL CASE is text but every arm
// evaluates to NULL anyway.
func coerceCase(v Value, toDecimal bool) Value {
	if toDecimal && v.Kind == ValInt {
		return DecimalValue(DecimalFromInt64(v.Int))
	}
	return v
}

// requireAssignable: a value assigned to a column must match its family — an integer column
// takes an integer (or NULL) value; a decimal column takes an integer (int→decimal implicit) or
// decimal (or NULL) value; a text column takes a text (or NULL) value; a boolean column takes a
// boolean (or NULL) value. A decimal value into an integer column is NOT assignable (decimal→int
// is explicit-CAST only). Any cross-family pair is a 42804 type error. Mirrors the INSERT literal
// type-check, generalized to expressions.
func requireAssignable(t resolvedType, colTy ScalarType, col string) error {
	var ok bool
	switch {
	case colTy.IsBool():
		ok = t.kind == rtBool || t.kind == rtNull
	case colTy.IsInteger():
		ok = t.kind == rtInt || t.kind == rtNull
	case colTy.IsDecimal():
		ok = t.kind == rtInt || t.kind == rtDecimal || t.kind == rtNull
	case colTy.IsBytea():
		ok = t.kind == rtBytea || t.kind == rtNull
	case colTy.IsUuid():
		ok = t.kind == rtUuid || t.kind == rtNull
	case colTy.IsTimestamp():
		ok = t.kind == rtTimestamp || t.kind == rtNull
	case colTy.IsTimestamptz():
		ok = t.kind == rtTimestamptz || t.kind == rtNull
	default: // text
		ok = t.kind == rtText || t.kind == rtNull
	}
	if !ok {
		return typeError("cannot assign a value to column " + col + " of type " + colTy.CanonicalName())
	}
	return nil
}

// resolveTypeAndTypmod resolves a column-definition or CAST target type name + optional type
// modifier. All canonical names and aliases (including boolean/bool and numeric/decimal/dec)
// resolve here; a genuinely unknown name is a 42704. A type modifier is meaningful only for
// decimal (validated to numeric(p,s) — 22023); on any other type it is 0A000 (varchar(n) and
// other parameterized types are deferred — spec/design/grammar.md §14). Type-specific narrowings
// (a text/boolean/decimal PRIMARY KEY, a CAST to text/boolean) are enforced at the call site.
func resolveTypeAndTypmod(name string, tm *TypeMod) (ScalarType, *DecimalTypmod, error) {
	ty, ok := ScalarTypeFromName(name)
	if !ok {
		return 0, nil, NewError(UndefinedObject, "type does not exist: "+name)
	}
	if tm == nil {
		return ty, nil, nil
	}
	if !ty.IsDecimal() {
		return 0, nil, NewError(FeatureNotSupported,
			"a type modifier is not supported for type "+ty.CanonicalName())
	}
	typmod, err := validateDecimalTypmod(tm)
	if err != nil {
		return 0, nil, err
	}
	return ty, typmod, nil
}

// validateDecimalTypmod validates a decimal numeric(p[,s]) type modifier: 1 <= p <= 1000,
// 0 <= s <= p; else trap 22023 (spec/design/decimal.md §2). numeric(p) means scale 0.
func validateDecimalTypmod(tm *TypeMod) (*DecimalTypmod, error) {
	p := tm.Precision
	if p < 1 || p > MaxPrecision {
		return nil, NewError(InvalidParameterValue,
			fmt.Sprintf("NUMERIC precision %d must be between 1 and %d", p, MaxPrecision))
	}
	var s uint64
	if tm.Scale != nil {
		s = *tm.Scale
	}
	if s > p || s > MaxScale {
		return nil, NewError(InvalidParameterValue,
			fmt.Sprintf("NUMERIC scale %d must be between 0 and precision %d", s, p))
	}
	return &DecimalTypmod{Precision: uint16(p), Scale: uint16(s)}, nil
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
func (e *rExpr) eval(row Row, params []Value, m *Meter) (Value, error) {
	switch e.kind {
	case reColumn:
		return row[e.index], nil
	case reParam:
		// The supplied value, already coerced to its inferred type by bindParams before
		// execution (spec/design/api.md §5).
		return params[e.index], nil
	case reConstInt:
		return IntValue(e.cInt), nil
	case reConstBool:
		return BoolValue(e.cBool), nil
	case reConstText:
		return TextValue(e.cText), nil
	case reConstDecimal:
		return DecimalValue(e.cDec), nil
	case reConstBytea:
		return ByteaValue(e.cBytea), nil
	case reConstUuid:
		return UuidValue(e.cBytea), nil
	case reConstTimestamp:
		return TimestampValue(e.cInt), nil
	case reConstTimestamptz:
		return TimestamptzValue(e.cInt), nil
	case reConstNull:
		return NullValue(), nil
	case reCast:
		m.Charge(Costs.OperatorEval)
		v, err := e.operand.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		return evalCast(v, e.result, e.typmod)
	case reNeg:
		m.Charge(Costs.OperatorEval)
		v, err := e.operand.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		if e.result.IsDecimal() {
			if v.Kind == ValInt {
				return DecimalValue(DecimalFromInt64(v.Int).Negate()), nil
			}
			return DecimalValue(v.Dec.Negate()), nil
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
		v, err := e.operand.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		return boolNot(v), nil
	case reArith:
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		if a.Kind == ValNull || b.Kind == ValNull {
			return NullValue(), nil
		}
		if e.result.IsDecimal() {
			// Decimal arithmetic: widen any integer operand to decimal, then apply the op with
			// PG's scale rules (spec/design/decimal.md §4).
			return evalDecimalArith(e.op, toDecimal(a), toDecimal(b))
		}
		return evalArith(e.op, a.Int, b.Int, e.result)
	case reCompare:
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, params, m)
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
		a, err := e.lhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		return boolAnd(a, b), nil
	case reOr:
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		return boolOr(a, b), nil
	case reIsNull:
		m.Charge(Costs.OperatorEval)
		v, err := e.operand.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		isNull := v.Kind == ValNull
		return BoolValue(isNull != e.negated), nil
	case reLike:
		m.Charge(Costs.OperatorEval)
		subject, err := e.lhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		pattern, err := e.rhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		// NULL propagates BEFORE the matcher runs, so a malformed pattern against a NULL operand
		// is still NULL, never 22025 (matches PG — grammar.md §22).
		if subject.Kind == ValNull || pattern.Kind == ValNull {
			return NullValue(), nil
		}
		matched, err := likeMatch(subject.Str, pattern.Str)
		if err != nil {
			return Value{}, err
		}
		// negated carries NOT LIKE: matched != negated flips the result for NOT LIKE.
		return BoolValue(matched != e.negated), nil
	case reCase:
		// CASE is the ONE deliberate exception to "no short-circuit" (cost.md §3): conditions are
		// evaluated in order and evaluation STOPS at the first TRUE — a FALSE or NULL/UNKNOWN
		// condition falls through, and later arms (and their results) are NOT evaluated. Required
		// for PG semantics (e.g. `CASE WHEN a=0 THEN 0 ELSE 1/a END` must not divide by zero).
		// Charge the node, then only the conditions up to the match plus the selected result.
		m.Charge(Costs.OperatorEval)
		for _, arm := range e.caseArms {
			cv, err := arm.cond.eval(row, params, m)
			if err != nil {
				return Value{}, err
			}
			if cv.Kind == ValBool && cv.Bool {
				rv, err := arm.result.eval(row, params, m)
				if err != nil {
					return Value{}, err
				}
				return coerceCase(rv, e.caseDecimal), nil
			}
		}
		ev, err := e.caseEls.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		return coerceCase(ev, e.caseDecimal), nil
	default: // reDistinct
		m.Charge(Costs.OperatorEval)
		a, err := e.lhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, params, m)
		if err != nil {
			return Value{}, err
		}
		// negated carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks "are they
		// the same?"; IS DISTINCT FROM asks the opposite. Always a definite boolean — never
		// unknown (the null_safe discipline, functions.md §3).
		return BoolValue(a.NotDistinctFrom(b) == e.negated), nil
	}
}

// likeMatch is the SQL LIKE matcher (spec/design/grammar.md §22): `%` matches any (possibly
// empty) run of characters, `_` exactly one character, and `\` (the default escape) makes the
// next pattern character literal. It iterates by Unicode code point (via []rune) so astral
// characters match `_` (a CLAUDE.md §8 determinism surface), via a two-pointer greedy
// backtracking walk identical across cores. It returns a 22025 error when the escape character
// is the LAST pattern character reached during matching (PostgreSQL's "LIKE pattern must not end
// with escape character") — data-dependent, since an earlier mismatch returns false first.
func likeMatch(subject, pattern string) (bool, error) {
	s := []rune(subject)
	p := []rune(pattern)
	si, pi := 0, 0
	// The last '%' position in the pattern (a backtrack point) and the subject index when it
	// was taken; -1 until a '%' has been seen.
	starPi, starSi := -1, 0
	for si < len(s) {
		switch {
		case pi < len(p) && p[pi] == '\\':
			// Escape: the next pattern character must match the subject literally.
			if pi+1 >= len(p) {
				return false, NewError(InvalidEscapeSequence, "LIKE pattern must not end with escape character")
			}
			if s[si] == p[pi+1] {
				si++
				pi += 2
				continue
			}
			// literal mismatch → fall through to backtrack
		case pi < len(p) && p[pi] == '_':
			si++
			pi++
			continue
		case pi < len(p) && p[pi] == '%':
			starPi = pi
			starSi = si
			pi++
			continue
		case pi < len(p) && p[pi] == s[si]:
			si++
			pi++
			continue
		}
		// Mismatch: backtrack to the last '%' (it absorbs one more subject character), else no.
		if starPi >= 0 {
			pi = starPi + 1
			starSi++
			si = starSi
			continue
		}
		return false, nil
	}
	// Subject consumed: any pattern remainder must be all '%' to match.
	for pi < len(p) && p[pi] == '%' {
		pi++
	}
	return pi == len(p), nil
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

// evalCast evaluates a (non-NULL) CAST to target. int→int range-checks (22003); int→decimal
// widens then coerces to the typmod; decimal→int rounds half-away to scale 0 then range-checks
// (22003); decimal→decimal re-scales to the typmod (spec/design/decimal.md §6).
func evalCast(v Value, target ScalarType, typmod *DecimalTypmod) (Value, error) {
	if v.Kind == ValInt {
		if target.IsDecimal() {
			d, err := coerceDecimal(DecimalFromInt64(v.Int), typmod)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(d), nil
		}
		if !target.InRange(v.Int) {
			return Value{}, overflowErr(target)
		}
		return IntValue(v.Int), nil
	}
	// v.Kind == ValDecimal
	if target.IsDecimal() {
		d, err := coerceDecimal(*v.Dec, typmod)
		if err != nil {
			return Value{}, err
		}
		return DecimalValue(d), nil
	}
	n, ok := v.Dec.ToInt64Round()
	if !ok || !target.InRange(n) {
		return Value{}, overflowErr(target)
	}
	return IntValue(n), nil
}

// toDecimal widens a numeric value to Decimal (an integer operand of decimal arithmetic).
func toDecimal(v Value) Decimal {
	if v.Kind == ValDecimal {
		return *v.Dec
	}
	return DecimalFromInt64(v.Int)
}

// evalDecimalArith evaluates decimal arithmetic with PG's result-scale rules
// (spec/design/decimal.md §4), trapping 22003 at the cap and 22012 on a zero divisor/modulus.
func evalDecimalArith(op BinaryOp, a, b Decimal) (Value, error) {
	var (
		r   Decimal
		err error
	)
	switch op {
	case OpAdd:
		r, err = a.Add(b)
	case OpSub:
		r, err = a.Sub(b)
	case OpMul:
		r, err = a.Mul(b)
	case OpDiv:
		r, err = a.Div(b)
	default: // OpMod
		r, err = a.Rem(b)
	}
	if err != nil {
		return Value{}, err
	}
	return DecimalValue(r), nil
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

// keyCmp is one ORDER BY key's total-order comparison, returning <0, 0, >0. NULL placement
// is governed by nullsFirst and applied INDEPENDENTLY of the value-direction flip
// (descending), so an explicit NULLS FIRST|LAST overrides the direction default
// (spec/design/grammar.md §10). The physical key order ratifies NULL as the largest value
// (the PostgreSQL model), which surfaces as the parse-time default nullsFirst = descending.
func keyCmp(a, b Value, descending, nullsFirst bool) int {
	switch {
	case a.Kind == ValNull && b.Kind == ValNull:
		return 0
	case a.Kind == ValNull:
		if nullsFirst {
			return -1
		}
		return 1
	case b.Kind == ValNull:
		if nullsFirst {
			return 1
		}
		return -1
	}
	base := valueCmp(a, b)
	if descending {
		return -base
	}
	return base
}

// valueCmp is the total order over NON-NULL values: signed-integer ascending, text by
// the C collation — raw UTF-8 bytes, which for UTF-8 equals code-point order (Go's
// strings.Compare is byte order — spec/design/types.md §11) — and boolean by value,
// false < true (orderKey maps false→0, true→1; types.md §9). The cross-family arms are
// defined only for totality — ORDER BY is over a single typed column, so a mixed pair is
// unreachable from SELECT. NULLs are handled by keyCmp before this is reached. Returns
// <0, 0, >0.
func valueCmp(a, b Value) int {
	switch {
	case a.Kind == ValInt && b.Kind == ValInt:
		return cmpInt64(a.Int, b.Int)
	case a.Kind == ValDecimal && b.Kind == ValDecimal:
		return a.Dec.CmpValue(*b.Dec)
	case a.Kind == ValText && b.Kind == ValText:
		return strings.Compare(a.Str, b.Str)
	case a.Kind == ValBytea && b.Kind == ValBytea:
		// bytea is held in Str (raw bytes); strings.Compare is unsigned byte order.
		return strings.Compare(a.Str, b.Str)
	case a.Kind == ValUuid && b.Kind == ValUuid:
		// uuid's 16 raw bytes are held in Str; strings.Compare is unsigned byte order.
		return strings.Compare(a.Str, b.Str)
	case a.Kind == ValBool && b.Kind == ValBool:
		return cmpInt64(orderKey(a), orderKey(b))
	case a.Kind == ValTimestamp && b.Kind == ValTimestamp:
		return cmpInt64(a.Int, b.Int)
	case a.Kind == ValTimestamptz && b.Kind == ValTimestamptz:
		return cmpInt64(a.Int, b.Int)
	default:
		// Cross-family arms exist only for totality — ORDER BY is over a single typed column,
		// so a mixed pair is unreachable. A fixed family order keeps the comparator total.
		return cmpInt64(int64(familyRank(a)), int64(familyRank(b)))
	}
}

func cmpInt64(x, y int64) int {
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

// familyRank is a fixed total order across value families, for the unreachable cross-family
// case of valueCmp (ORDER BY is single-column-typed).
func familyRank(v Value) int {
	switch v.Kind {
	case ValNull:
		return 0
	case ValBool:
		return 1
	case ValInt:
		return 2
	case ValDecimal:
		return 3
	case ValText:
		return 4
	case ValBytea:
		return 5
	case ValUuid:
		return 6
	case ValTimestamp:
		return 7
	case ValTimestamptz:
		return 8
	default:
		return 9
	}
}

// assignPlan is a resolved UPDATE assignment: the target column index, its type and
// nullability for re-checking, and the resolved RHS expression (evaluated against the
// old row).
type assignPlan struct {
	idx     int
	name    string
	target  ScalarType
	decimal *DecimalTypmod
	notNull bool
	source  *rExpr
}

// check type-checks + coerces a candidate value against this column — the same storeValue path
// INSERT uses (NULL into NOT NULL → 23502; an integer out of range → 22003; an integer into a
// decimal column widens to the typmod; a decimal rounds to scale; a boolean into a boolean
// column is accepted as-is). The resolver proved the value's family is assignable.
func (p assignPlan) check(v Value) (Value, error) {
	return storeValue(v, p.target, p.decimal, p.notNull, p.name)
}

// storeValue coerces a value into a column for storage (shared by INSERT and UPDATE). NULL
// honours NOT NULL (23502); an integer into an integer column is range-checked (22003); an
// integer into a decimal column widens (int→decimal) then coerces to the typmod; a decimal into
// a decimal column coerces to the typmod (rounds to scale, precision-checks → 22003); a
// cross-family value (decimal→int, text→int, etc.) is a 42804 (decimal→int is explicit-CAST only).
func storeValue(v Value, colTy ScalarType, typmod *DecimalTypmod, notNull bool, colName string) (Value, error) {
	switch v.Kind {
	case ValNull:
		if notNull {
			return Value{}, NewError(NotNullViolation,
				"null value in column "+colName+" violates not-null constraint")
		}
		return NullValue(), nil
	case ValInt:
		if colTy.IsInteger() {
			if !colTy.InRange(v.Int) {
				return Value{}, overflowErr(colTy)
			}
			return IntValue(v.Int), nil
		}
		if colTy.IsDecimal() {
			d, err := coerceDecimal(DecimalFromInt64(v.Int), typmod)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(d), nil
		}
		return Value{}, typeError("cannot store an integer value in " + colTy.CanonicalName() + " column " + colName)
	case ValDecimal:
		if colTy.IsDecimal() {
			d, err := coerceDecimal(*v.Dec, typmod)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(d), nil
		}
		return Value{}, typeError("cannot store a decimal value in " + colTy.CanonicalName() + " column " + colName)
	case ValText:
		if colTy.IsText() {
			return TextValue(v.Str), nil
		}
		if colTy.IsBytea() {
			// A string literal adapts to a bytea column, decoding the hex input form
			// (types.md §6/§13); malformed hex traps 22P02.
			b, err := decodeByteaLiteral(v.Str)
			if err != nil {
				return Value{}, err
			}
			return ByteaValue(b), nil
		}
		if colTy.IsUuid() {
			// A string literal adapts to a uuid column via the PG-flexible input
			// (types.md §6/§14); malformed input traps 22P02.
			b, err := decodeUUIDLiteral(v.Str)
			if err != nil {
				return Value{}, err
			}
			return UuidValue(b), nil
		}
		if colTy.IsTimestamp() {
			// A string literal adapts to a timestamp column (spec/design/timestamp.md);
			// malformed input traps 22007, an out-of-range field 22008.
			m, err := ParseTimestamp(v.Str)
			if err != nil {
				return Value{}, err
			}
			return TimestampValue(m), nil
		}
		if colTy.IsTimestamptz() {
			m, err := ParseTimestamptz(v.Str)
			if err != nil {
				return Value{}, err
			}
			return TimestamptzValue(m), nil
		}
		return Value{}, typeError("cannot store a text value in " + colTy.CanonicalName() + " column " + colName)
	case ValBytea:
		if colTy.IsBytea() {
			return v, nil
		}
		return Value{}, typeError("cannot store a bytea value in " + colTy.CanonicalName() + " column " + colName)
	case ValUuid:
		if colTy.IsUuid() {
			return v, nil
		}
		return Value{}, typeError("cannot store a uuid value in " + colTy.CanonicalName() + " column " + colName)
	case ValTimestamp:
		if colTy.IsTimestamp() {
			return v, nil
		}
		return Value{}, typeError("cannot store a timestamp value in " + colTy.CanonicalName() + " column " + colName)
	case ValTimestamptz:
		if colTy.IsTimestamptz() {
			return v, nil
		}
		return Value{}, typeError("cannot store a timestamptz value in " + colTy.CanonicalName() + " column " + colName)
	default: // ValBool
		if colTy.IsBool() {
			return BoolValue(v.Bool), nil
		}
		return Value{}, typeError("cannot store a boolean value in " + colTy.CanonicalName() + " column " + colName)
	}
}

// coerceDecimal coerces a decimal into a column's typmod: round to the declared scale and
// precision-check (22003) for numeric(p,s); for an unconstrained numeric column just cap-check.
func coerceDecimal(d Decimal, typmod *DecimalTypmod) (Decimal, error) {
	if typmod != nil {
		return d.CoerceToTypmod(uint32(typmod.Precision), uint32(typmod.Scale))
	}
	return d.CheckCap()
}

// literalToValue wraps a parsed literal as a runtime value (type-check/coercion is storeValue).
func literalToValue(lit Literal) Value {
	switch lit.Kind {
	case LiteralNull:
		return NullValue()
	case LiteralInt:
		return IntValue(lit.Int)
	case LiteralBool:
		return BoolValue(lit.Bool)
	case LiteralText:
		return TextValue(lit.Str)
	default: // LiteralDecimal
		return DecimalValue(lit.Dec)
	}
}
