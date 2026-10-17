package jed

// CREATE TABLE … AS query [WITH [NO] DATA] (spec/design/create-table-as.md): a table whose columns are
// the query's output columns and whose rows are the query's rows. The query is planned once to derive
// the columns — each output's name (or the column list's), type, typmod, and collation — then the
// table is created through CREATE TABLE's shared registration, and WITH DATA fills it by running
// `INSERT INTO <table> <query>` in the same statement, so the rows, the cost, and the errors are
// exactly that INSERT's. Mirrors impl/rust create_table_as.rs and impl/ts create_table_as.ts.

import (
	"fmt"
	"slices"
)

// executeCreateTableAs executes a `CREATE TABLE … AS` (create-table-as.md §3). The checks run in
// PostgreSQL's order: the target database and the jed_ reservation, then everything planning the
// query raises, then a taken name (IF NOT EXISTS skips here, after the query was analyzed), then per
// column an underivable collation (42P22), then too many column names (42601), ON COMMIT on a table
// that is not temporary (42P16), a repeated column name (42701), and a column type no table can have
// (42P16 for an anonymous record, 0A000 for jed's own narrowings).
func (db *engine) executeCreateTableAs(ct *createTable, params []Value) (outcome, error) {
	plan, ok, err := db.ctasPrepare(ct, params)
	if err != nil {
		return outcome{}, err
	}
	if !ok {
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}
	cta := ct.AsQuery
	targetTemp, attachName, err := db.createTableTarget(ct)
	if err != nil {
		return outcome{}, err
	}
	columns, err := db.ctasColumns(ct, &plan, targetTemp)
	if err != nil {
		return outcome{}, err
	}
	if _, err := db.createTableWithColumns(ct, columns); err != nil {
		return outcome{}, err
	}
	if !cta.WithData {
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}
	// Fill the new table: the rows, cost, and errors are exactly `INSERT INTO t <query>`'s. The target
	// is named in its own database, so no other relation of the same name can capture it.
	target := "main"
	switch {
	case attachName != "":
		target = attachName
	case targetTemp:
		target = "temp"
	}
	insertOf := func(source queryExpr) *insert {
		return &insert{Table: ct.Name, DB: &target, Query: &source}
	}
	// A data-modifying WITH is the statement's own (create-table-as.md §1): the fill is
	// `WITH … INSERT INTO t <body>`, run by the writable-CTE orchestrator, so every part reads the
	// pre-statement snapshot (with the new, empty table) and runs exactly once.
	if w := cta.Query.With; w != nil && withIsDataModifying(w) {
		wq := &withQuery{Ctes: w.Ctes, Body: cteBody{Insert: insertOf(*w.Body)}, Recursive: w.Recursive}
		return db.executeWithDml(wq, params)
	}
	return db.executeInsertCached(insertOf(cta.Query), params, cteCtx{}, nil, false)
}

// ctasPrepare runs the checks of a `CREATE TABLE … AS` before anything is created (create-table-as.md
// §3 steps 2–4), shared with `EXPLAIN CREATE TABLE … AS` (§7): the target database and the jed_
// reservation, the query planned with its parameters typed and bound, and the name check. It returns
// the planned query, and false when IF NOT EXISTS skips a taken name.
func (db *engine) ctasPrepare(ct *createTable, params []Value) (queryPlan, bool, error) {
	cta := ct.AsQuery
	_, attachName, err := db.createTableTarget(ct)
	if err != nil {
		return queryPlan{}, false, err
	}
	if err := checkReservedName("table", ct.Name); err != nil {
		return queryPlan{}, false, err
	}
	// Plan the query once, for its output columns. Its $N parameters are typed and bound as a query's
	// are, so a wrong parameter count or an untyped parameter fails here.
	ptypes := &paramTypes{}
	var plan queryPlan
	if w := cta.Query.With; w != nil && withIsDataModifying(w) {
		// A data-modifying WITH plans as the statement's own WITH (writable-cte.md): its CTEs first,
		// then the body that reads them.
		body := cteBody{Query: w.Body}
		bindings, err := db.planCteBindings(w.Ctes, w.Recursive, nil, nil, ptypes, ctePrimary{body: &body})
		if err != nil {
			return queryPlan{}, false, err
		}
		if plan, err = db.planQuery(*w.Body, nil, bindings, ptypes); err != nil {
			return queryPlan{}, false, err
		}
	} else if plan, err = db.planQuery(cta.Query, nil, nil, ptypes); err != nil {
		return queryPlan{}, false, err
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return queryPlan{}, false, err
	}
	if _, err := bindParams(params, ptys, db.inputZone()); err != nil {
		return queryPlan{}, false, err
	}
	if db.createTableNameTaken(ct.Name, attachName) {
		if ct.IfNotExists {
			return queryPlan{}, false, nil
		}
		return queryPlan{}, false, newError(DuplicateTable, "relation already exists: "+ct.Name)
	}
	return plan, true, nil
}

// withIsDataModifying reports whether a `CREATE TABLE … AS` query's own WITH holds a data-modifying
// CTE (create-table-as.md §1), which makes it the statement's writable WITH.
func withIsDataModifying(w *withExpr) bool {
	for i := range w.Ctes {
		if w.Ctes[i].Body.IsDataModifying() {
			return true
		}
	}
	return false
}

// ctasQueryStatement is the query statement a `CREATE TABLE … AS` runs (create-table-as.md §7): its
// query as a top-level SELECT, set operation, or WITH — what `EXPLAIN CREATE TABLE … AS` renders.
func ctasQueryStatement(q queryExpr) statement {
	switch {
	case q.Select != nil:
		return statement{Select: q.Select}
	case q.SetOp != nil:
		return statement{SetOp: q.SetOp}
	default:
		w := q.With
		return statement{With: &withQuery{Ctes: w.Ctes, Body: cteBody{Query: w.Body}, Recursive: w.Recursive}}
	}
}

// ctasColumns derives the new table's columns from the planned query (create-table-as.md §2): the
// column list's names over the query's output names, the output types (an untyped NULL is text), the
// typmods the outputs keep, and each text column's collation. A table column has no constraint,
// default, or key.
func (db *engine) ctasColumns(ct *createTable, plan *queryPlan, targetTemp bool) ([]catColumn, error) {
	types := plan.columnTypes()
	derivs := plan.columnDerivs()
	typmods := plan.columnTypmods()
	renames := ct.AsQuery.ColumnNames
	names := make([]string, len(plan.columnNames()))
	for i, n := range plan.columnNames() {
		if i < len(renames) {
			names[i] = renames[i]
		} else {
			names[i] = n
		}
	}
	// A text column whose collation could not be derived (two different implicit collations met in a
	// UNION ALL, VALUES, or expression) cannot be created: PostgreSQL would otherwise give it a default
	// it never chose.
	for i, t := range types {
		if t.collatable() && (derivs[i].kind == derivIndeterminate || derivs[i].kind == derivConflicted) {
			return nil, newError(IndeterminateCollation, fmt.Sprintf(
				"no collation was derived for column \"%s\" with collatable type %s", names[i], rtName(t),
			)).hinted(hintIndeterminateCollation)
		}
	}
	if len(renames) > len(types) {
		return nil, newError(SyntaxError, "too many column names were specified")
	}
	if ct.OnCommit != nil && !targetTemp {
		return nil, newError(InvalidTableDefinition, "ON COMMIT can only be used on temporary tables")
	}
	for i, name := range names {
		if slices.Contains(names[:i], name) {
			return nil, newError(DuplicateColumn, "duplicate column name: "+name)
		}
	}
	columns := make([]catColumn, 0, len(types))
	for i, rt := range types {
		ty, err := ctasColumnType(names[i], rt)
		if err != nil {
			return nil, err
		}
		var tm typmod
		if _, ok := ty.AsScalar(); ok {
			tm = typmods[i]
		}
		collation := ""
		if ty.IsCollatable() {
			switch d := derivs[i]; d.kind {
			case derivImplicit, derivExplicit:
				if d.name != "C" {
					collation = d.name
				}
			default:
				// A column that derives no collation (a literal, an untyped NULL) takes the database's
				// default, as a declared column without COLLATE does.
				collation = db.readSnap().defaultCollation
			}
		}
		columns = append(columns, catColumn{
			Name:       names[i],
			Type:       ty,
			Decimal:    tm.Decimal,
			VarcharLen: tm.VarcharLen,
			Collation:  collation,
		})
	}
	return columns, nil
}

// rejectPseudoType rejects a persisted relation column (a CREATE TABLE … AS table's or a view's)
// whose type is an anonymous ROW(…) or an array of one: PostgreSQL's pseudo-type record / record[],
// which only a query's own output may carry (spec/design/composite.md §1) — 42P16.
func rejectPseudoType(name string, rt resolvedType) error {
	anonymous := func(t resolvedType) bool { return t.kind == rtComposite && !t.comp.named }
	pseudo := ""
	switch {
	case anonymous(rt):
		pseudo = "record"
	case rt.kind == rtArray && anonymous(*rt.elem):
		pseudo = "record[]"
	default:
		return nil
	}
	return newError(InvalidTableDefinition, fmt.Sprintf("column \"%s\" has pseudo-type %s", name, pseudo))
}

// ctasColumnType is the table-column type of a query output column of resolved type rt
// (create-table-as.md §2): an untyped NULL is text, as in PostgreSQL. An anonymous ROW(…) (or an array
// of one) has no storable type — PostgreSQL's pseudo-type record, 42P16. The types a declared column
// cannot have in jed either are the same 0A000: a jsonpath column, and an array whose element is
// itself an array or a range.
func ctasColumnType(name string, rt resolvedType) (dataType, error) {
	if err := rejectPseudoType(name, rt); err != nil {
		return dataType{}, err
	}
	ty, err := typeFromResolved(rt)
	if err != nil {
		return dataType{}, err
	}
	supported := true
	switch {
	case ty.Array != nil:
		supported = ty.Array.Array == nil && ty.Array.Range == nil && ty.Array.Record == nil
	case ty.Record != nil:
		// Unreachable: rejectPseudoType rejects a record (and a record[]) first.
		supported = false
	case ty.Comp == nil && ty.Range == nil:
		supported = ty.Scalar != scalarJsonPath
	}
	if !supported {
		return dataType{}, newError(FeatureNotSupported, "a "+ty.CanonicalName()+" column is not supported yet")
	}
	return ty, nil
}
