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
	case stmt.DropTable != nil:
		return db.executeDropTable(stmt.DropTable)
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
		ty, decimal, err := resolveTypeAndTypmod(def.TypeName, def.TypeMod)
		if err != nil {
			return Outcome{}, err
		}
		if def.PrimaryKey {
			// Only integers may be a key this slice. The order-preserving text and decimal key
			// encodings (spec/design/encoding.md §2.4/§2.5) are authored but unexercised, so a
			// text or decimal PRIMARY KEY is a documented 0A000 narrowing (types.md §11/§12).
			if !ty.IsInteger() {
				return Outcome{}, NewError(FeatureNotSupported,
					"a "+ty.CanonicalName()+" primary key is not supported yet")
			}
			// Likewise boolean: the bool-byte key encoding rule is authored but
			// unexercised, so a boolean PRIMARY KEY is a documented 0A000 narrowing
			// (spec/design/types.md §9), relaxable in a later boolean-in-key slice.
			if ty.IsBool() {
				return Outcome{}, NewError(FeatureNotSupported,
					"a boolean primary key is not supported yet")
			}
			if pkSeen {
				return Outcome{}, NewError(InvalidTableDefinition,
					"a table may have at most one primary key")
			}
			pkSeen = true
		}
		columns = append(columns, Column{
			Name:       def.Name,
			Type:       ty,
			Decimal:    decimal,
			PrimaryKey: def.PrimaryKey,
			NotNull:    def.PrimaryKey, // PRIMARY KEY ⇒ NOT NULL
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

// executeInsert analyzes and runs an INSERT of one or more rows. Each row maps its
// literal values positionally to columns and is type-checked (NULL into NOT NULL traps
// 23502; an integer outside the column type's range traps 22003 — CLAUDE.md §8); a
// duplicate primary key traps 23505. A multi-row INSERT is two-phase / all-or-nothing
// (spec/design/grammar.md §12), mirroring UPDATE: every row is validated — including its
// storage key checked against both the stored rows and earlier rows in the same statement
// — before any row is inserted, so a mid-batch failure stores nothing.
func (db *Database) executeInsert(ins *Insert) (Outcome, error) {
	table, ok := db.Table(ins.Table)
	if !ok {
		return Outcome{}, NewError(UndefinedTable, "table does not exist: "+ins.Table)
	}
	store := db.stores[strings.ToLower(ins.Table)]
	pk := table.PrimaryKeyIndex()

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
	for _, lits := range ins.Rows {
		if len(lits) != len(table.Columns) {
			return Outcome{}, NewError(SyntaxError, fmt.Sprintf(
				"INSERT row has %d values but table %s has %d columns",
				len(lits), table.Name, len(table.Columns),
			))
		}

		row := make(Row, len(table.Columns))
		for i, col := range table.Columns {
			// The literal adapts/coerces to its target column: an integer literal into a
			// decimal column widens (int→decimal, then to the typmod); a decimal literal into a
			// decimal column rounds to its scale; a cross-family pair is 42804
			// (spec/design/decimal.md §6, types.md §5).
			v, err := storeValue(literalToValue(lits[i]), col.Type, col.Decimal, col.NotNull, col.Name)
			if err != nil {
				return Outcome{}, err
			}
			row[i] = v
		}

		var key []byte
		if pk >= 0 {
			key = EncodeInt(table.Columns[pk].Type, row[pk].Int)
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
	// INSERT of literal rows reads no rows and evaluates no expression tree: zero cost
	// (DEFAULT expressions, when added, will accrue here).
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
	// DELETE is single-table; resolve its WHERE against a one-relation scope.
	s := singleScope(table)
	var filter *rExpr
	if del.Filter != nil {
		f, err := resolveBooleanFilter(s, del.Filter)
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
	// UPDATE is single-table; the RHS / WHERE resolve against a one-relation scope so the
	// shared resolver serves it too (a qualified `WHERE t.a` against the sole table is fine).
	s := singleScope(table)

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
		src, ty, err := resolve(s, a.Value, &col.Type)
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
		f, err := resolveBooleanFilter(s, upd.Filter)
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
	projections, columnNames, err := resolveProjections(s, sel.Items)
	if err != nil {
		return Outcome{}, err
	}
	var filter *rExpr
	if sel.Filter != nil {
		filter, err = resolveBooleanFilter(s, sel.Filter)
		if err != nil {
			return Outcome{}, err
		}
	}
	type orderKeyPlan struct {
		idx        int
		descending bool
		nullsFirst bool
	}
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
		order = append(order, orderKeyPlan{idx: idx, descending: key.Descending, nullsFirst: key.NullsFirst})
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
			on, oerr := resolveBooleanFilter(partial, j.On)
			if oerr != nil {
				return Outcome{}, oerr
			}
			joinOns[k] = on
		}
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
					v, err := on.eval(combined, meter)
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

	// ORDER BY: stable sort applying each key left to right — the first non-equal key decides,
	// and a full tie keeps the scan order (SliceStable). Each key's NULL placement is decoupled
	// from its value-direction flip (spec/design/grammar.md §10).
	if len(order) > 0 {
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
	if sel.Distinct {
		// Project every filtered row (charging projection cost per row, the §3 asymmetry),
		// keeping first occurrences. `seen` is membership-only: output order comes from the
		// deterministic source iteration, never from map iteration (no map-order leak —
		// CLAUDE.md §8/§10).
		seen := make(map[string]bool)
		var distinctRows [][]Value
		for _, row := range rows {
			projected := make([]Value, len(projections))
			for i, p := range projections {
				v, err := p.eval(row, meter)
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
				v, err := p.eval(row, meter)
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
	rtText    // text (one family, collation C); does not promote
	rtDecimal // decimal (one family; the per-column typmod is carried separately)
	rtBytea   // bytea (one family, raw bytes); does not promote
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

// ctxOf returns the type a sibling operand offers an adaptable literal: an integer type
// (so an integer literal adopts that width), or bytea/text (so a string literal can decode
// to bytea, else stay text). nil for bool/decimal/NULL — no useful literal context.
func ctxOf(t resolvedType) *ScalarType {
	switch t.kind {
	case rtInt:
		ty := t.intTy
		return &ty
	case rtBytea:
		ty := Bytea
		return &ty
	case rtText:
		ty := Text
		return &ty
	default:
		return nil
	}
}

// rExprKind tags a resolved expression node.
type rExprKind int

const (
	reColumn rExprKind = iota
	reConstInt
	reConstBool
	reConstText
	reConstDecimal
	reConstBytea
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
	default:
		return resolvedType{kind: rtInt, intTy: ty}
	}
}

// resolveProjections resolves SELECT items into evaluable projections (any result type is
// allowed in the select list, including boolean — SELECT a = b), each paired with its output
// column name (spec/design/grammar.md §8). `*` expands across ALL relations in FROM order,
// each relation's columns in catalog order (§15).
func resolveProjections(s *scope, items SelectItems) ([]*rExpr, []string, error) {
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
		node, _, err := resolve(s, it.Expr, nil)
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
	default:
		return "?column?"
	}
}

// resolveBooleanFilter resolves a WHERE / ON expression; it must resolve to boolean (or an
// untyped NULL, which is always unknown → no rows). An integer- or text-valued one is 42804.
func resolveBooleanFilter(s *scope, e *Expr) (*rExpr, error) {
	node, ty, err := resolve(s, *e, nil)
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
func resolve(s *scope, e Expr, ctx *ScalarType) (*rExpr, resolvedType, error) {
	switch e.Kind {
	case ExprColumn:
		idx, err := s.resolveBare(e.Column)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reColumn, index: idx}, resolvedTypeOf(s.columnAt(idx).Type), nil
	case ExprQualifiedColumn:
		idx, err := s.resolveQualified(e.Qualifier, e.Column)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reColumn, index: idx}, resolvedTypeOf(s.columnAt(idx).Type), nil
	case ExprLiteral:
		switch e.Literal.Kind {
		case LiteralNull:
			return &rExpr{kind: reConstNull}, resolvedType{kind: rtNull}, nil
		case LiteralBool:
			return &rExpr{kind: reConstBool, cBool: e.Literal.Bool}, resolvedType{kind: rtBool}, nil
		case LiteralText:
			// A string literal is text by default (collation C). It adapts to a BYTEA context
			// only (types.md §6/§13): decode the hex input there (22P02 on bad hex); any other
			// context — including none — keeps it text.
			if ctx != nil && ctx.IsBytea() {
				b, err := decodeByteaLiteral(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstBytea, cBytea: b}, resolvedType{kind: rtBytea}, nil
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
		inner, ity, err := resolve(s, e.Cast.Inner, nil)
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
		// int→int (range check), int→decimal (widen), decimal→int (explicit, round),
		// decimal→decimal (re-scale), and NULL are all castable.
		resultRt := resolvedType{kind: rtInt, intTy: target}
		if target.IsDecimal() {
			resultRt = resolvedType{kind: rtDecimal}
		}
		return &rExpr{kind: reCast, operand: inner, result: target, typmod: typmod}, resultRt, nil
	case ExprUnary:
		if e.Unary.Op == OpNeg {
			rop, ty, err := resolve(s, e.Unary.Operand, ctx)
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
		rop, ty, err := resolve(s, e.Unary.Operand, nil)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireBool(ty, "NOT requires a boolean operand"); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reNot, operand: rop}, resolvedType{kind: rtBool}, nil
	case ExprIsNull:
		rop, _, err := resolve(s, e.IsNullOf.Operand, nil)
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
		rl, lt, rr, rt, err := resolveOperandPair(s, e.IsDistinct.Lhs, e.IsDistinct.Rhs)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := classifyComparable(lt, rt); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reDistinct, lhs: rl, rhs: rr, negated: e.IsDistinct.Negated},
			resolvedType{kind: rtBool}, nil
	default: // ExprBinary
		return resolveBinary(s, e.Binary)
	}
}

func resolveBinary(s *scope, b *BinaryExpr) (*rExpr, resolvedType, error) {
	switch b.Op {
	case OpAdd, OpSub, OpMul, OpDiv, OpMod:
		// Arithmetic is overloaded across integer and decimal. Resolve the operand pair (an
		// integer literal adapts to an integer sibling), then pick the family: both integer →
		// integer arithmetic; at least one decimal → decimal arithmetic (the integer operand
		// widens at eval); a text/boolean operand is a 42804 (spec/design/decimal.md §4).
		rl, lt, rr, rt, err := resolveOperandPair(s, b.Lhs, b.Rhs)
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
		rl, lt, rr, rt, err := resolveOperandPair(s, b.Lhs, b.Rhs)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := classifyComparable(lt, rt); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reCompare, op: b.Op, lhs: rl, rhs: rr},
			resolvedType{kind: rtBool}, nil
	default: // OpAnd, OpOr
		rl, lt, err := resolve(s, b.Lhs, nil)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rr, rt, err := resolve(s, b.Rhs, nil)
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
func resolveOperandPair(s *scope, lhs, rhs Expr) (*rExpr, resolvedType, *rExpr, resolvedType, error) {
	lhsLit := isAdaptableLiteral(lhs)
	rhsLit := isAdaptableLiteral(rhs)
	var rl, rr *rExpr
	var lt, rt resolvedType
	var err error
	switch {
	case lhsLit && rhsLit:
		i64 := Int64
		if rl, lt, err = resolve(s, lhs, &i64); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(s, rhs, &i64)
	case lhsLit:
		if rr, rt, err = resolve(s, rhs, nil); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rl, lt, err = resolve(s, lhs, ctxOf(rt))
	case rhsLit:
		if rl, lt, err = resolve(s, lhs, nil); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(s, rhs, ctxOf(lt))
	default:
		if rl, lt, err = resolve(s, lhs, nil); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(s, rhs, nil)
	}
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	return rl, lt, rr, rt, nil
}

// requireNumericOperand requires that an arithmetic operand is numeric (integer or decimal,
// or NULL); a boolean or text operand is a 42804 type error.
func requireNumericOperand(t resolvedType) error {
	if t.kind == rtBool || t.kind == rtText || t.kind == rtBytea {
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
	return nil
}

// isAdaptableLiteral reports whether e is a literal that adapts to its sibling operand's
// type (an integer or string literal). NULL, boolean, and decimal literals do not take a
// sibling's context here.
func isAdaptableLiteral(e Expr) bool {
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
	if t.kind == rtInt || t.kind == rtText || t.kind == rtDecimal || t.kind == rtBytea {
		return typeError(msg)
	}
	return nil
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
func (e *rExpr) eval(row Row, m *Meter) (Value, error) {
	switch e.kind {
	case reColumn:
		return row[e.index], nil
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
		return evalCast(v, e.result, e.typmod)
	case reNeg:
		m.Charge(Costs.OperatorEval)
		v, err := e.operand.eval(row, m)
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
		if e.result.IsDecimal() {
			// Decimal arithmetic: widen any integer operand to decimal, then apply the op with
			// PG's scale rules (spec/design/decimal.md §4).
			return evalDecimalArith(e.op, toDecimal(a), toDecimal(b))
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
	case a.Kind == ValBool && b.Kind == ValBool:
		return cmpInt64(orderKey(a), orderKey(b))
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
	default: // ValBytea
		return 5
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
		return Value{}, typeError("cannot store a text value in " + colTy.CanonicalName() + " column " + colName)
	case ValBytea:
		if colTy.IsBytea() {
			return v, nil
		}
		return Value{}, typeError("cannot store a bytea value in " + colTy.CanonicalName() + " column " + colName)
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
