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
	cta := ct.AsQuery
	targetTemp, attachName, err := db.createTableTarget(ct)
	if err != nil {
		return outcome{}, err
	}
	if err := checkReservedName("table", ct.Name); err != nil {
		return outcome{}, err
	}
	// Plan the query once, for its output columns. Its $N parameters are typed and bound as a query's
	// are, so a wrong parameter count or an untyped parameter fails here.
	ptypes := &paramTypes{}
	plan, err := db.planQuery(cta.Query, nil, nil, ptypes)
	if err != nil {
		return outcome{}, err
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return outcome{}, err
	}
	if _, err := bindParams(params, ptys, db.inputZone()); err != nil {
		return outcome{}, err
	}
	if db.createTableNameTaken(ct.Name, attachName) {
		if ct.IfNotExists {
			return outcome{Kind: outcomeStatement, Cost: 0}, nil
		}
		return outcome{}, newError(DuplicateTable, "relation already exists: "+ct.Name)
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
	query := cta.Query
	ins := &insert{Table: ct.Name, DB: &target, Query: &query}
	return db.executeInsertCached(ins, params, cteCtx{}, nil, false)
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
			))
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
		if ty.Comp == nil && ty.Array == nil && ty.Range == nil {
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

// ctasColumnType is the table-column type of a query output column of resolved type rt
// (create-table-as.md §2): an untyped NULL is text, as in PostgreSQL. An anonymous ROW(…) (or an array
// of one) has no storable type — PostgreSQL's pseudo-type record, 42P16. The types a declared column
// cannot have in jed either are the same 0A000: a jsonpath column, and an array whose element is
// itself an array or a range.
func ctasColumnType(name string, rt resolvedType) (dataType, error) {
	anonymous := func(t resolvedType) bool { return t.kind == rtComposite && !t.comp.named }
	pseudo := ""
	switch {
	case anonymous(rt):
		pseudo = "record"
	case rt.kind == rtArray && anonymous(*rt.elem):
		pseudo = "record[]"
	}
	if pseudo != "" {
		return dataType{}, newError(InvalidTableDefinition, fmt.Sprintf("column \"%s\" has pseudo-type %s", name, pseudo))
	}
	ty, err := typeFromResolved(rt)
	if err != nil {
		return dataType{}, err
	}
	supported := true
	switch {
	case ty.Array != nil:
		supported = ty.Array.Array == nil && ty.Array.Range == nil
	case ty.Comp == nil && ty.Range == nil:
		supported = ty.Scalar != scalarJsonPath
	}
	if !supported {
		return dataType{}, newError(FeatureNotSupported, "a "+ty.CanonicalName()+" column is not supported yet")
	}
	return ty, nil
}
