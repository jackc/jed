package jed

import (
	"fmt"
	"strings"
)

// Set-returning functions in FROM — planning and row production. This file holds the resolvers that
// turn a table-position function call into a synthetic table + srfPlan (resolveSRF and the per-family
// resolvers: generate_series, unnest, the JSON each/record/populate functions, JSON_TABLE, and the
// jed_* catalog relations), and the matching row producers (generateSeriesRows/unnestRows/jsonSrfRows/
// jsonTableRows and the jed{Tables,Columns,Indexes,Constraints}Rows introspection sources), plus the
// JSON_TABLE column-tree machinery.

// resolveSRF resolves a FROM-clause set-returning function call (generate_series(...)) into a
// SYNTHETIC one-column relation plus its resolved argument expressions (spec/design/functions.md
// §10). Only generate_series exists this slice (any other name → 42883), with 2 or 3 integer
// args (a wrong arity/type → 42883). Non-LATERAL: the args resolve against an EMPTY-local-rels
// scope whose parent is the enclosing query, so $N and correlated outer columns resolve while a
// sibling FROM table does not (42703/42P01). The produced column is typed at the PROMOTED integer
// type of the args (PG); a NULL-typed arg contributes no width. Its NAME follows PostgreSQL's
// single-column function-alias rule: the table alias when one is given (generate_series(1,5) AS g
// ⇒ column g), else the function name generate_series.
func (db *engine) resolveSRF(name string, args []*exprNode, alias *string, columnDefs []typeFieldDef, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	// The args see only params/outer — never sibling FROM tables (non-LATERAL); CTE bindings are
	// inherited so an arg subquery can reference a CTE (cte.md §2).
	argScope := &scope{rels: nil, parent: parent, catalog: db, allowSubquery: true, ctes: ctes}
	lname := strings.ToLower(name)
	// Record-returning functions (R1, json-table.md §2): json[b]_to_record → one record row,
	// json[b]_to_recordset → setof record. They take their column shape from the C0 col-def list
	// `AS t(col type, …)`. Dispatched first, before the col-def-list guard below.
	switch lname {
	case "json_to_record", "jsonb_to_record", "json_to_recordset", "jsonb_to_recordset":
		jsonb := strings.HasPrefix(lname, "jsonb")
		set := strings.HasSuffix(lname, "set")
		return db.resolveJSONRecord(lname, jsonb, set, args, alias, columnDefs, argScope, ptypes)
	// json[b]_populate_record(set) (R2, json-table.md §2): like json[b]_to_record(set) but the
	// column shape comes from the COMPOSITE TYPE of the (typically NULL) first argument.
	case "json_populate_record", "jsonb_populate_record", "json_populate_recordset", "jsonb_populate_recordset":
		jsonb := strings.HasPrefix(lname, "jsonb")
		set := strings.HasSuffix(lname, "set")
		return db.resolveJSONPopulate(lname, jsonb, set, args, alias, argScope, ptypes)
	}
	// A column-definition list is valid ONLY on a record-returning function (PG).
	if columnDefs != nil {
		return nil, nil, newError(SyntaxError,
			"a column definition list is only allowed for a record-returning function, not "+name)
	}
	switch {
	case strings.EqualFold(name, "generate_series"):
		return db.resolveGenerateSeries(args, alias, argScope, ptypes)
	case strings.EqualFold(name, "unnest"):
		return db.resolveUnnest(args, alias, argScope, ptypes)
	}
	// json/jsonb two-column SRFs (B3, json-sql-functions.md §3): jsonb_each → (key text, value
	// jsonb), jsonb_each_text → (key text, value text). The json variants (verbatim sub-text,
	// json.md §4) are a deferred 0A000 follow-on. Built on the C0 multi-column synthetic table.
	switch lname {
	case "jsonb_each":
		return db.resolveJSONEach(lname, srfJsonbEach, scalarT(scalarJsonb), args, alias, argScope, ptypes)
	case "jsonb_each_text":
		return db.resolveJSONEach(lname, srfJsonbEachText, scalarT(scalarText), args, alias, argScope, ptypes)
	case "json_each", "json_each_text":
		return nil, nil, newError(FeatureNotSupported, lname+" is not supported yet; use the jsonb variant")
	}
	// json/jsonb single-column SRFs (B2, json-sql-functions.md §3). The json `array_elements`
	// variants preserve the verbatim sub-text (json.md §4) and are a deferred 0A000 follow-on, like
	// the json accessor operators; the jsonb variants + `json_object_keys` ship here.
	switch lname {
	case "jsonb_array_elements":
		return db.resolveJSONSrf(lname, srfJsonbArrayElements, scalarT(scalarJsonb), true, args, alias, argScope, ptypes)
	case "jsonb_array_elements_text":
		return db.resolveJSONSrf(lname, srfJsonbArrayElementsText, scalarT(scalarText), true, args, alias, argScope, ptypes)
	case "jsonb_object_keys":
		return db.resolveJSONSrf(lname, srfJsonbObjectKeys, scalarT(scalarText), true, args, alias, argScope, ptypes)
	case "json_object_keys":
		return db.resolveJSONSrf(lname, srfJsonObjectKeys, scalarT(scalarText), false, args, alias, argScope, ptypes)
	case "json_array_elements", "json_array_elements_text":
		return nil, nil, newError(FeatureNotSupported, lname+" is not supported yet; use the jsonb variant")
	}
	// jsonb_path_query(jsonb, jsonpath) (P2, jsonpath.md §5.2): one `jsonb` row per item of the path's
	// evaluation sequence over the context document. A bare string literal adapts (the ctx to jsonb,
	// the path to a compiled jsonpath). STRICT in the args; a NULL ctx/path → zero rows at exec.
	if lname == "jsonb_path_query" {
		forbidden := &aggCtx{}
		ctx, path, err := resolveJsonpathArgs(argScope, lname, args, forbidden, ptypes)
		if err != nil {
			return nil, nil, err
		}
		return srfTable(lname, alias, scalarT(scalarJsonb)), &srfPlan{kind: srfJsonbPathQuery, args: []*rExpr{ctx, path}}, nil
	}
	return nil, nil, newError(UndefinedFunction, "function does not exist: "+name)
}

// resolveJSONSrf resolves a json/jsonb single-column SRF (B2, json-sql-functions.md §3): the one
// argument is a json/jsonb value (a bare string literal adapts to the expected document type). The
// synthetic column's type is fixed (`jsonb`/`text`). A NULL argument yields zero rows at exec.
func (db *engine) resolveJSONSrf(name string, kind srfKind, colTy dataType, jsonb bool, args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 1 {
		return nil, nil, noFuncOverload(name)
	}
	want := scalarJson
	if jsonb {
		want = scalarJsonb
	}
	forbidden := &aggCtx{}
	r, t, err := resolve(argScope, *args[0], &want, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	ok := t.kind == rtNull || (jsonb && t.kind == rtJsonb) || (!jsonb && t.kind == rtJson)
	if !ok {
		return nil, nil, noFuncOverload(name)
	}
	return srfTable(name, alias, colTy), &srfPlan{kind: kind, args: []*rExpr{r}}, nil
}

// resolveJSONEach resolves a json/jsonb TWO-column SRF (B3 — jsonb_each / jsonb_each_text,
// json-sql-functions.md §3): the one argument is a jsonb value (a bare string literal adapts). The
// synthetic relation has the fixed columns `key text` and `value <valueTy>` (the C0 multi-column
// synthetic table). A non-object argument → 22023 at exec; a NULL → zero rows.
func (db *engine) resolveJSONEach(name string, kind srfKind, valueTy dataType, args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 1 {
		return nil, nil, noFuncOverload(name)
	}
	want := scalarJsonb
	forbidden := &aggCtx{}
	r, t, err := resolve(argScope, *args[0], &want, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	if t.kind != rtJsonb && t.kind != rtNull {
		return nil, nil, noFuncOverload(name)
	}
	table := srfTableCols(name, alias, []srfCol{{"key", scalarT(scalarText)}, {"value", valueTy}})
	return table, &srfPlan{kind: kind, args: []*rExpr{r}}, nil
}

// resolveJSONRecord resolves a json/jsonb RECORD-returning SRF (R1 — json[b]_to_record /
// json[b]_to_recordset, json-table.md §2): the one argument is a json/jsonb document; the output
// columns come from the C0 col-def list `AS t(col type, …)` (required — else 42601). The synthetic
// table's columns are the declared types (a composite/array column type is a deferred 0A000), and
// the srfPlan carries them as recordCols so the row generator can map members → columns by name.
func (db *engine) resolveJSONRecord(name string, jsonb, set bool, args []*exprNode, alias *string, columnDefs []typeFieldDef, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 1 {
		return nil, nil, noFuncOverload(name)
	}
	want := scalarJson
	if jsonb {
		want = scalarJsonb
	}
	forbidden := &aggCtx{}
	r, t, err := resolve(argScope, *args[0], &want, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	ok := t.kind == rtNull || (jsonb && t.kind == rtJsonb) || (!jsonb && t.kind == rtJson)
	if !ok {
		return nil, nil, noFuncOverload(name)
	}
	if columnDefs == nil {
		return nil, nil, newError(SyntaxError,
			"a column definition list is required for function "+name)
	}
	columns := make([]catColumn, 0, len(columnDefs))
	for _, d := range columnDefs {
		// A composite/array column type in the col-def list is a deferred 0A000 follow-on.
		if strings.HasSuffix(d.TypeName, "[]") || db.CompositeType(d.TypeName) != nil {
			return nil, nil, newError(FeatureNotSupported,
				"a composite/array column in a record column-definition list is not supported yet")
		}
		st, decimal, varcharLen, err := resolveTypeAndTypmod(d.TypeName, d.TypeMod)
		if err != nil {
			return nil, nil, err
		}
		columns = append(columns, catColumn{Name: d.Name, Type: scalarT(st), Decimal: decimal, VarcharLen: varcharLen})
	}
	tname := name
	if alias != nil {
		tname = *alias
	}
	table := &catTable{Name: tname, Columns: columns}
	kind := srfJSONRecord
	if set {
		kind = srfJSONRecordset
	}
	return table, &srfPlan{kind: kind, args: []*rExpr{r}, recordCols: columns}, nil
}

// resolveJSONPopulate resolves a json/jsonb POPULATE-RECORD SRF (R2 — json[b]_populate_record(set),
// json-table.md §2): the FIRST argument is a (typically NULL) value whose COMPOSITE TYPE supplies
// the output column shape; the SECOND is the json/jsonb document. Reuses the R1 row machinery
// (srfJSONRecord(set)) — only the column source differs (a composite type vs a col-def list). A
// non-composite first argument → 42804; an anonymous record base → 0A000.
func (db *engine) resolveJSONPopulate(name string, jsonb, set bool, args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 2 {
		return nil, nil, noFuncOverload(name)
	}
	forbidden := &aggCtx{}
	// The base argument's COMPOSITE type fixes the columns (its value is unused — usually NULL).
	_, bt, err := resolve(argScope, *args[0], nil, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	if bt.kind != rtComposite {
		return nil, nil, newError(DatatypeMismatch,
			"the first argument of "+name+" must be a composite type")
	}
	// A named composite supplies the columns; an anonymous record base is 0A000.
	if !bt.comp.named {
		return nil, nil, newError(FeatureNotSupported, "an anonymous record base is not supported yet")
	}
	ctype := db.CompositeType(bt.comp.name)
	if ctype == nil {
		return nil, nil, newError(UndefinedObject, "composite type no longer exists")
	}
	columns := make([]catColumn, 0, len(ctype.Fields))
	for _, f := range ctype.Fields {
		columns = append(columns, catColumn{Name: f.Name, Type: f.Type, Decimal: f.Decimal, VarcharLen: f.VarcharLen})
	}
	// The SECOND argument is the json/jsonb document.
	want := scalarJson
	if jsonb {
		want = scalarJsonb
	}
	r, dt, err := resolve(argScope, *args[1], &want, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	ok := dt.kind == rtNull || (jsonb && dt.kind == rtJsonb) || (!jsonb && dt.kind == rtJson)
	if !ok {
		return nil, nil, noFuncOverload(name)
	}
	tname := name
	if alias != nil {
		tname = *alias
	}
	table := &catTable{Name: tname, Columns: columns}
	kind := srfJSONRecord
	if set {
		kind = srfJSONRecordset
	}
	// The SRF arg is the json DOCUMENT (the base value is unused); reuse the R1 row generator.
	return table, &srfPlan{kind: kind, args: []*rExpr{r}, recordCols: columns}, nil
}

// resolveJSONTable resolves a JSON_TABLE(ctx, path COLUMNS (…)) source (T1, json-table.md §3) → its
// synthetic relation (the flattened columns), the `[ctx]` arg, and the resolved jtPlan. The ctx /
// root path see only params + the lateral prefix (never sibling columns of THIS relation) — an
// empty-local-rels scope chained to `parent`, exactly like an SRF (grammar.md §44).
func (db *engine) resolveJSONTable(jt *jsonTable, alias *string, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	argScope := &scope{rels: nil, parent: parent, catalog: db, allowSubquery: true, ctes: ctes}
	forbidden := &aggCtx{}
	// The context item (json / jsonb / text, coerced to a jsonb document at eval).
	jsonbHint := scalarJsonb
	rctx, ctxTy, err := resolve(argScope, *jt.Ctx, &jsonbHint, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	switch ctxTy.kind {
	case rtJsonb, rtJson, rtText, rtNull:
		// ok
	default:
		return nil, nil, newError(DatatypeMismatch,
			fmt.Sprintf("the context item of JSON_TABLE must be json/jsonb/text, not %s", rtName(ctxTy)))
	}
	// The root path — a constant jsonpath (a string literal compiles to a reConstJsonPath node).
	pathHint := scalarJsonPath
	rpath, pathTy, err := resolve(argScope, *jt.Path, &pathHint, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	if pathTy.kind != rtJsonPath {
		return nil, nil, newError(DatatypeMismatch, "the path of JSON_TABLE must be a constant jsonpath")
	}
	if rpath.kind != reConstJsonPath {
		return nil, nil, newError(FeatureNotSupported, "a non-constant JSON_TABLE path is not supported")
	}
	rootPath := rpath.cText
	var outColumns []catColumn
	columns, err := db.resolveJtColumns(jt.Columns, &outColumns)
	if err != nil {
		return nil, nil, err
	}
	tname := "json_table"
	if alias != nil {
		tname = *alias
	}
	table := &catTable{Name: tname, Columns: outColumns}
	return table, &srfPlan{
		kind:      srfJsonTable,
		args:      []*rExpr{rctx},
		jsonTable: &jtPlan{rootPath: rootPath, width: len(outColumns), columns: columns},
	}, nil
}

// resolveJtColumns recursively resolves a JSON_TABLE COLUMNS tree, flattening the leaf columns into
// `outColumns` (pre-order, declaration order) and assigning each its flat output index.
func (db *engine) resolveJtColumns(cols []jtColumn, outColumns *[]catColumn) ([]jtCol, error) {
	resolved := make([]jtCol, 0, len(cols))
	for _, col := range cols {
		switch c := col.(type) {
		case *jtColumnOrdinality:
			idx := len(*outColumns)
			*outColumns = append(*outColumns, newJtColumn(c.Name, scalarInt32, nil))
			resolved = append(resolved, &jtColOrdinality{idx: idx})
		case *jtColumnRegular:
			if c.Array {
				return nil, newError(FeatureNotSupported, "an array JSON_TABLE column is not supported yet")
			}
			st, decimal, err := jtScalarType(db, c.TypeName)
			if err != nil {
				return nil, err
			}
			if !c.KeepQuotes {
				return nil, newError(FeatureNotSupported, "JSON_TABLE OMIT QUOTES is not supported yet")
			}
			query := st == scalarJson || st == scalarJsonb
			if !query && c.Wrapper != jWWithout {
				return nil, newError(FeatureNotSupported, "a WRAPPER on a scalar JSON_TABLE column is not supported yet")
			}
			compiled, err := jtCompilePath(c.Path, c.Name)
			if err != nil {
				return nil, err
			}
			idx := len(*outColumns)
			*outColumns = append(*outColumns, newJtColumn(c.Name, st, decimal))
			resolved = append(resolved, &jtColRegular{
				idx:       idx,
				returning: st,
				decimal:   decimal,
				path:      compiled,
				query:     query,
				wrapper:   c.Wrapper,
				onEmpty:   jtBehavior(c.OnEmpty, jOBNull),
				onError:   jtBehavior(c.OnError, jOBNull),
			})
		case *jtColumnExists:
			st, _, err := jtScalarType(db, c.TypeName)
			if err != nil {
				return nil, err
			}
			compiled, err := jtCompilePath(c.Path, c.Name)
			if err != nil {
				return nil, err
			}
			idx := len(*outColumns)
			*outColumns = append(*outColumns, newJtColumn(c.Name, st, nil))
			resolved = append(resolved, &jtColExists{
				idx:       idx,
				returning: st,
				path:      compiled,
				onError:   jtBehavior(c.OnError, jOBFalse),
			})
		case *jtColumnNested:
			compiled, err := compile(c.Path)
			if err != nil {
				return nil, err
			}
			nested, err := db.resolveJtColumns(c.Columns, outColumns)
			if err != nil {
				return nil, err
			}
			resolved = append(resolved, &jtColNested{path: compiled.Render(), columns: nested})
		default:
			panic("resolveJtColumns: unknown JtColumn kind")
		}
	}
	return resolved, nil
}

// resolveGenerateSeries resolves generate_series(start, stop[, step]) (spec/design/functions.md
// §10): 2 or 3 integer args (a wrong arity/type → 42883). The produced column is typed at the
// PROMOTED integer type of the args (PG); a NULL-typed arg contributes no width. All-NULL defaults
// i64.
func (db *engine) resolveGenerateSeries(args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 2 && len(args) != 3 {
		return nil, nil, noFuncOverload("generate_series")
	}
	int64Ctx := scalarInt64
	forbidden := &aggCtx{}
	rargs := make([]*rExpr, 0, len(args))
	var result scalarType
	haveResult := false
	for _, a := range args {
		r, t, err := resolve(argScope, *a, &int64Ctx, forbidden, ptypes)
		if err != nil {
			return nil, nil, err
		}
		switch t.kind {
		case rtInt:
			if !haveResult || t.intTy.Rank() > result.Rank() {
				result = t.intTy
				haveResult = true
			}
		case rtNull:
			// An untyped NULL/param adapts and contributes no width.
		default:
			return nil, nil, noFuncOverload("generate_series")
		}
		rargs = append(rargs, r)
	}
	if !haveResult {
		result = scalarInt64
	}
	return srfTable("generate_series", alias, scalarT(result)), &srfPlan{kind: srfGenerateSeries, args: rargs}, nil
}

// resolveUnnest resolves unnest(anyarray) (spec/design/array-functions.md §9, §13): the single
// argument must be an array (binding ELEM := its element type, the produced column's type), else
// 42883 (a non-array, e.g. unnest(5)). A bare untyped NULL argument leaves ELEM undeterminable →
// 42P18 (jed's polymorphic posture, like array_append(NULL, NULL)); a typed NULL array
// (NULL::i32[]) resolves and yields zero rows at exec. ELEM may be a scalar OR a composite (AF7 —
// unnest(composite[])): the synthetic column is typed at the bound element type directly
// (typeFromResolved), so a composite array produces composite rows (an anonymous-composite element
// has no catalog name → 0A000, not reachable from a typed array).
func (db *engine) resolveUnnest(args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 1 {
		return nil, nil, noFuncOverload("unnest")
	}
	forbidden := &aggCtx{}
	r, t, err := resolve(argScope, *args[0], nil, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	switch t.kind {
	case rtArray:
		elemTy, err := typeFromResolved(*t.elem)
		if err != nil {
			return nil, nil, err
		}
		return srfTable("unnest", alias, elemTy), &srfPlan{kind: srfUnnest, args: []*rExpr{r}}, nil
	case rtNull:
		return nil, nil, indeterminatePoly()
	default:
		return nil, nil, noFuncOverload("unnest")
	}
}

// srfTable builds a set-returning function's SYNTHETIC one-column relation (spec/design/functions.md
// §10). The table's Name is the function name (the un-aliased label fallback); the lone column's
// NAME follows PostgreSQL's single-column function-alias rule — the table alias when one is given,
// else the function name — and its TYPE is colTy (the promoted integer for generate_series, the
// bound element type for unnest).
func srfTable(funcName string, alias *string, colTy dataType) *catTable {
	colName := funcName
	if alias != nil {
		colName = *alias
	}
	return &catTable{
		Name:    funcName,
		Columns: []catColumn{{Name: colName, Type: colTy}},
	}
}

// srfCol is one fixed column of a multi-column SRF synthetic table (its name + type).
type srfCol struct {
	name string
	ty   dataType
}

// srfTableCols builds a MULTI-COLUMN synthetic table for a set-returning function (C0,
// json-table.md §1) — the generalization of srfTable to N named/typed columns. The column NAMES are
// fixed by the function (e.g. jsonb_each → key, value); the FROM alias renames the RELATION (the
// table Name), not its columns. Used by json[b]_each[_text] (and, with a col-def list, the record
// functions).
func srfTableCols(funcName string, alias *string, cols []srfCol) *catTable {
	name := funcName
	if alias != nil {
		name = *alias
	}
	columns := make([]catColumn, len(cols))
	for i, c := range cols {
		columns[i] = catColumn{Name: c.name, Type: c.ty}
	}
	return &catTable{Name: name, Columns: columns}
}

// srfKindName is the catalog name of a json two-column SRF, for its non-object error message.
func srfKindName(kind srfKind) string {
	switch kind {
	case srfJsonbEach:
		return "jsonb_each"
	case srfJsonbEachText:
		return "jsonb_each_text"
	default:
		panic("srfKindName is only for the json two-column SRFs")
	}
}

// catalogRelKind classifies a relation name as a built-in catalog relation (introspection.md §5):
// jed_tables / jed_columns, case-insensitively (identifier resolution folds case; grammar.md §3
// leaves no quoted escape). Built-in names resolve in every database's relation namespace, checked
// AFTER a statement-local CTE (a CTE shadows a catalog relation — PG-matching, oracle-checked) and
// BEFORE the user catalog (post-I0 the two can never collide; for a pre-reservation legacy file
// the built-in wins and the user relation is unreachable by name — §5).
func catalogRelKind(name string) (srfKind, bool) {
	switch strings.ToLower(name) {
	case "jed_tables":
		return srfJedTables, true
	case "jed_columns":
		return srfJedColumns, true
	case "jed_indexes":
		return srfJedIndexes, true
	case "jed_constraints":
		return srfJedConstraints, true
	}
	return 0, false
}

// indexMethodName is the access-method name rendered by jed_indexes.method (introspection.md §5.1):
// the PostgreSQL amname spelling of the index kind.
func indexMethodName(kind indexKind) string {
	switch kind {
	case indexGin:
		return "gin"
	case indexGist:
		return "gist"
	default:
		return "btree"
	}
}

// isCatalogRelName reports whether name is a built-in catalog relation (jed_tables / jed_columns).
// The write paths use it to reject a catalog relation as a mutation/DDL target (42809 — a catalog
// relation is read-only, introspection.md §5); the privilege gate uses it so a built-in is
// SELECT-gated exactly like a user table under an explicit-grant session envelope.
func isCatalogRelName(name string) bool { _, ok := catalogRelKind(name); return ok }

// checkCatalogRelWrite rejects a mutation target (INSERT / UPDATE / DELETE / CREATE INDEX ON)
// naming a built-in catalog relation: 42809 wrong_object_type, `cannot modify system relation`
// (introspection.md §5 — the relations are read-only computed views of the catalog). Checked by
// NAME, before qualifier validation: the built-in resolves in every database's namespace, so the
// rejection is scope-independent.
func checkCatalogRelWrite(name string) error {
	if isCatalogRelName(name) {
		return newError(WrongObjectType,
			`cannot modify system relation "`+strings.ToLower(name)+`"`)
	}
	return nil
}

// catalogRelTable builds the FIXED synthetic schema of a catalog relation (introspection.md §5).
// Unlike an SRF's single-column alias rule, a FROM alias renames the RELATION only — the column
// names are part of the introspection surface. Growth is by ADDING columns (consumers select by
// name, not position — §5).
func catalogRelTable(kind srfKind) *catTable {
	textArr := arrayT(scalarT(scalarText)) // a text[] member-list column (introspection.md §5.1)
	switch kind {
	case srfJedTables:
		return &catTable{Name: "jed_tables", Columns: []catColumn{
			{Name: "name", Type: scalarT(scalarText), NotNull: true},
		}}
	case srfJedColumns:
		return &catTable{Name: "jed_columns", Columns: []catColumn{
			{Name: "table_name", Type: scalarT(scalarText), NotNull: true},
			{Name: "name", Type: scalarT(scalarText), NotNull: true},
			{Name: "ordinal", Type: scalarT(scalarInt32), NotNull: true},
			{Name: "type", Type: scalarT(scalarText), NotNull: true},
			{Name: "not_null", Type: scalarT(scalarBool), NotNull: true},
			{Name: "pk_ordinal", Type: scalarT(scalarInt32)},
		}}
	case srfJedIndexes:
		return &catTable{Name: "jed_indexes", Columns: []catColumn{
			{Name: "name", Type: scalarT(scalarText), NotNull: true},
			{Name: "table_name", Type: scalarT(scalarText), NotNull: true},
			{Name: "columns", Type: textArr, NotNull: true},
			{Name: "is_unique", Type: scalarT(scalarBool), NotNull: true},
			{Name: "method", Type: scalarT(scalarText), NotNull: true},
		}}
	default: // srfJedConstraints
		return &catTable{Name: "jed_constraints", Columns: []catColumn{
			{Name: "name", Type: scalarT(scalarText), NotNull: true},
			{Name: "table_name", Type: scalarT(scalarText), NotNull: true},
			{Name: "type", Type: scalarT(scalarText), NotNull: true},
			{Name: "columns", Type: textArr},
			{Name: "expression", Type: scalarT(scalarText)},
			{Name: "ref_table", Type: scalarT(scalarText)},
			{Name: "ref_columns", Type: textArr},
		}}
	}
}

// resolveCatalogScope validates a catalog relation's database qualifier and returns the scope
// string snapForScope resolves at exec (introspection.md §5): nil (unqualified) ⇒ "main" (the
// implicit scope); "main"/"temp" pass; any other qualifier must name a host attachment (else
// 42P01, the checkTableQualifier wording). Unlike a user table there is no per-table existence
// half — the relation exists in EVERY valid scope, so only the scope itself is validated.
func (db *engine) resolveCatalogScope(qualifier *string) (string, error) {
	if qualifier == nil {
		return "main", nil
	}
	q := strings.ToLower(*qualifier)
	if q == "main" || q == "temp" {
		return q, nil
	}
	if db.attachReadSnap(q) == nil {
		return "", newError(UndefinedTable, `database "`+*qualifier+`" is not attached`)
	}
	return q, nil
}

// catalogTypeText renders a column's declared type in the CANONICAL introspection form
// (introspection.md §5): the scalar's canonical name with its typmod applied at the leaf
// (varchar(10), decimal(8,2)), a composite's name as created, a range's canonical id (i32range,
// numrange, …), and `[]` appended for an array (the typmod applies to the element: varchar(5)[]).
// This text is a compatibility surface the moment it ships — pinned by the corpus.
func catalogTypeText(ty dataType, dec *decimalTypmod, vlen *uint32) string {
	if ty.Array != nil {
		return catalogTypeText(*ty.Array, dec, vlen) + "[]"
	}
	if ty.Range != nil {
		desc, _ := rangeForElement(ty.Range.ScalarTy())
		return desc.ID
	}
	if ty.Comp != nil {
		return ty.Comp.Name
	}
	if ty.Scalar == scalarText && vlen != nil {
		return fmt.Sprintf("varchar(%d)", *vlen)
	}
	if ty.Scalar == scalarDecimal && dec != nil {
		return fmt.Sprintf("decimal(%d,%d)", dec.Precision, dec.Scale)
	}
	return ty.Scalar.CanonicalName()
}

// jedTablesRows generates the rows of the jed_tables catalog relation (introspection.md §5): one
// row per USER table of the scope's pinned catalog snapshot — the canonical (CREATE TABLE-spelled)
// name — in ascending lowercased-name order (deterministic, no map-iteration leak; the multiset is
// the contract, order without ORDER BY stays unspecified — CLAUDE.md §8). Derived entirely from
// the resident catalog: zero page_read / storage_row_read; each produced row charges one
// generated_row AT THE SOURCE, guarded so a max_cost ceiling aborts deterministically (§13).
func (db *engine) jedTablesRows(sp *srfPlan, m *costMeter) ([]storedRow, error) {
	snap := db.snapForScope(sp.introspectScope)
	if snap == nil {
		// The attachment was valid at plan time but is gone at exec (a detached-then-reused plan).
		return nil, newError(UndefinedTable, `database "`+sp.introspectScope+`" is not attached`)
	}
	var out []storedRow
	for _, t := range snap.tablesSorted() {
		if err := m.Guard(); err != nil {
			return nil, err
		}
		m.Charge(costs.GeneratedRow)
		out = append(out, storedRow{TextValue(t.Name)})
	}
	return out, nil
}

// jedColumnsRows generates the rows of the jed_columns catalog relation (introspection.md §5): one
// row per column of every user table of the scope's snapshot, in (lowercased table name, ordinal)
// order. ordinal is 1-based CREATE TABLE order; type is the canonical type text (catalogTypeText);
// not_null covers a declared NOT NULL and PRIMARY KEY membership; pk_ordinal is the 1-based
// position in the PRIMARY KEY in KEY order (which may differ from declaration order —
// constraints.md §3), NULL for a non-member. Cost mirrors jedTablesRows.
func (db *engine) jedColumnsRows(sp *srfPlan, m *costMeter) ([]storedRow, error) {
	snap := db.snapForScope(sp.introspectScope)
	if snap == nil {
		return nil, newError(UndefinedTable, `database "`+sp.introspectScope+`" is not attached`)
	}
	var out []storedRow
	for _, t := range snap.tablesSorted() {
		for i, c := range t.Columns {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			pkOrdinal := NullValue()
			for k, ord := range t.PK {
				if ord == i {
					pkOrdinal = IntValue(int64(k + 1))
					break
				}
			}
			out = append(out, storedRow{
				TextValue(t.Name),
				TextValue(c.Name),
				IntValue(int64(i + 1)),
				TextValue(catalogTypeText(c.Type, c.Decimal, c.VarcharLen)),
				BoolValue(c.NotNull || c.PrimaryKey),
				pkOrdinal,
			})
		}
	}
	return out, nil
}

// jedIndexesRows generates the rows of the jed_indexes catalog relation (introspection.md §5.1):
// one row per secondary index of every user table of the scope's snapshot, in (lowercased table
// name, then the catalog's ascending index-name order) order. columns is the text[] of indexed
// column names in index-key order (duplicates included); is_unique the catalog flag; method the
// access-method name (btree/gin/gist). Cost mirrors jedTablesRows.
func (db *engine) jedIndexesRows(sp *srfPlan, m *costMeter) ([]storedRow, error) {
	snap := db.snapForScope(sp.introspectScope)
	if snap == nil {
		return nil, newError(UndefinedTable, `database "`+sp.introspectScope+`" is not attached`)
	}
	var out []storedRow
	for _, t := range snap.tablesSorted() {
		for _, idx := range t.Indexes {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			// A column key shows its column name; an expression key its canonical text
			// (introspection.md §5.1) — the same columns text[] cell.
			cols := make([]Value, len(idx.Keys))
			for j, k := range idx.Keys {
				if k.Expr != nil {
					cols[j] = TextValue(k.Expr.ExprText)
				} else {
					cols[j] = TextValue(t.Columns[k.Col].Name)
				}
			}
			out = append(out, storedRow{
				TextValue(idx.Name),
				TextValue(t.Name),
				ArrayValue(cols),
				BoolValue(idx.Unique),
				TextValue(indexMethodName(idx.Kind)),
			})
		}
	}
	return out, nil
}

// jedConstraintsRows generates the rows of the jed_constraints catalog relation (introspection.md
// §5.1): one row per CHECK / UNIQUE / FK / EXCLUDE constraint of every user table of the scope's
// snapshot, in (lowercased table name, then a fixed KIND order — check, unique, foreign_key,
// exclude — each already held in ascending lowercased-name order). PRIMARY KEY / NOT NULL are
// deliberately absent (they own no named object and are described by jed_columns). A UNIQUE
// constraint IS its backing unique b-tree index (constraints.md §5), so type='unique' lists every
// unique index; expression is the persisted canonical CHECK text (constraints.md §4.5). Cost
// mirrors jedTablesRows.
func (db *engine) jedConstraintsRows(sp *srfPlan, m *costMeter) ([]storedRow, error) {
	snap := db.snapForScope(sp.introspectScope)
	if snap == nil {
		return nil, newError(UndefinedTable, `database "`+sp.introspectScope+`" is not attached`)
	}
	textArr := func(names []string) Value {
		vals := make([]Value, len(names))
		for i, n := range names {
			vals[i] = TextValue(n)
		}
		return ArrayValue(vals)
	}
	var out []storedRow
	for _, t := range snap.tablesSorted() {
		// CHECK: name / table / 'check' / NULL columns / expression text / NULL ref_*.
		for _, ck := range t.Checks {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			out = append(out, storedRow{
				TextValue(ck.Name),
				TextValue(t.Name),
				TextValue("check"),
				NullValue(),
				TextValue(ck.ExprText),
				NullValue(),
				NullValue(),
			})
		}
		// UNIQUE: every unique b-tree index (a UNIQUE constraint IS its unique index).
		for _, idx := range t.Indexes {
			if !idx.Unique {
				continue
			}
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			// A column key shows its column name; an expression key its canonical text.
			cols := make([]string, len(idx.Keys))
			for j, k := range idx.Keys {
				if k.Expr != nil {
					cols[j] = k.Expr.ExprText
				} else {
					cols[j] = t.Columns[k.Col].Name
				}
			}
			out = append(out, storedRow{
				TextValue(idx.Name),
				TextValue(t.Name),
				TextValue("unique"),
				textArr(cols),
				NullValue(),
				NullValue(),
				NullValue(),
			})
		}
		// FOREIGN KEY: local columns / referenced (parent) table + columns (rendered from the
		// parent's canonical names — the parent always exists, it cannot be dropped while referenced,
		// constraints.md §6.10).
		for _, fk := range t.ForeignKeys {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			local := make([]string, len(fk.Columns))
			for j, ord := range fk.Columns {
				local[j] = t.Columns[ord].Name
			}
			parent, _ := snap.table(fk.RefTable)
			refTable := fk.RefTable
			if parent != nil {
				refTable = parent.Name
			}
			refCols := make([]string, len(fk.RefColumns))
			for j, ord := range fk.RefColumns {
				if parent != nil && ord < len(parent.Columns) {
					refCols[j] = parent.Columns[ord].Name
				}
			}
			out = append(out, storedRow{
				TextValue(fk.Name),
				TextValue(t.Name),
				TextValue("foreign_key"),
				textArr(local),
				NullValue(),
				TextValue(refTable),
				textArr(refCols),
			})
		}
		// EXCLUDE: the excluded columns in element order (the &&/= operators are a deferred column
		// addition — introspection.md §5.1).
		for _, exc := range t.Exclusions {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			cols := make([]string, len(exc.Elements))
			for j, el := range exc.Elements {
				cols[j] = t.Columns[el.Column].Name
			}
			out = append(out, storedRow{
				TextValue(exc.Name),
				TextValue(t.Name),
				TextValue("exclude"),
				textArr(cols),
				NullValue(),
				NullValue(),
				NullValue(),
			})
		}
	}
	return out, nil
}

// generateSeriesRows generates the rows of a generate_series(start, stop[, step]) FROM-clause
// source (spec/design/functions.md §10), as one-column rows. The args evaluate ONCE against the
// outer environment with no local row (non-LATERAL). PostgreSQL semantics: any NULL arg → zero
// rows; a step of zero → 22023; start > stop with a positive step (or the reverse) → zero rows;
// an i64 overflow while stepping STOPS the series cleanly (no trap). Each generated element
// charges one generated_row AT THE SOURCE, guarded so a max_cost ceiling aborts a runaway series
// (54P01) mid-generation before the whole thing materializes (CLAUDE.md §13).
func (db *engine) generateSeriesRows(sp *srfPlan, env *evalEnv, m *costMeter) ([]storedRow, error) {
	evalInt := func(e *rExpr) (int64, bool, error) {
		v, err := e.eval(nil, env, m)
		if err != nil {
			return 0, false, err
		}
		switch v.Kind {
		case ValInt:
			return v.Int, true, nil
		case ValNull:
			return 0, false, nil
		default:
			panic("the resolver restricts generate_series args to integers")
		}
	}
	start, okStart, err := evalInt(sp.args[0])
	if err != nil {
		return nil, err
	}
	stop, okStop, err := evalInt(sp.args[1])
	if err != nil {
		return nil, err
	}
	step, okStep := int64(1), true
	if len(sp.args) == 3 {
		step, okStep, err = evalInt(sp.args[2])
		if err != nil {
			return nil, err
		}
	}
	// Any NULL argument yields zero rows (PG).
	if !okStart || !okStop || !okStep {
		return nil, nil
	}
	if step == 0 {
		return nil, newError(InvalidParameterValue, "step size cannot be equal to zero")
	}
	var out []storedRow
	cur := start
	for {
		inRange := false
		if step > 0 {
			inRange = cur <= stop
		} else {
			inRange = cur >= stop
		}
		if !inRange {
			break
		}
		if err := m.Guard(); err != nil {
			return nil, err
		}
		m.Charge(costs.GeneratedRow)
		out = append(out, storedRow{IntValue(cur)})
		// i64 overflow while stepping ends the series cleanly, matching PostgreSQL.
		next := cur + step
		if (step > 0 && next < cur) || (step < 0 && next > cur) {
			break
		}
		cur = next
	}
	return out, nil
}

// jsonSrfRows generates the rows of a json/jsonb single-column SRF (B2, json-sql-functions.md §3). A
// NULL argument yields zero rows (empty_on_null). array_elements[_text] over a non-array, or
// object_keys over a non-object, is 22023. Each produced row charges one generated_row.
func (db *engine) jsonSrfRows(sp *srfPlan, env *evalEnv, m *costMeter) ([]storedRow, error) {
	arg, err := sp.args[0].eval(nil, env, m)
	if err != nil {
		return nil, err
	}
	if arg.Kind == ValNull {
		return nil, nil
	}
	node, err := jsonArgNode(arg)
	if err != nil {
		return nil, err
	}
	var out []storedRow
	switch sp.kind {
	case srfJsonbArrayElements, srfJsonbArrayElementsText:
		if node.Kind != JArray {
			return nil, newError(InvalidParameterValue, "cannot extract elements from a scalar")
		}
		for i := range node.Arr {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			e := node.Arr[i]
			var v Value
			if sp.kind == srfJsonbArrayElementsText {
				if s, ok := jsonNodeToText(&e); ok {
					v = TextValue(s)
				} else {
					v = NullValue()
				}
			} else {
				v = JsonbValue(e)
			}
			out = append(out, storedRow{v})
		}
	case srfJsonbObjectKeys, srfJsonObjectKeys:
		if node.Kind != JObject {
			return nil, newError(InvalidParameterValue, "cannot call jsonb_object_keys on a non-object")
		}
		for i := range node.Obj {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			out = append(out, storedRow{TextValue(node.Obj[i].Key)})
		}
	case srfJsonbEach, srfJsonbEachText:
		if node.Kind != JObject {
			return nil, newError(InvalidParameterValue, "cannot call "+srfKindName(sp.kind)+" on a non-object")
		}
		for i := range node.Obj {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			// (key text, value): jsonb_each keeps the value node; _text renders ->>-style
			// (a string member's raw content, a JSON null → SQL NULL, else canonical).
			var value Value
			if sp.kind == srfJsonbEachText {
				if s, ok := jsonNodeToText(&node.Obj[i].Val); ok {
					value = TextValue(s)
				} else {
					value = NullValue()
				}
			} else {
				value = JsonbValue(node.Obj[i].Val)
			}
			out = append(out, storedRow{TextValue(node.Obj[i].Key), value})
		}
	case srfJSONRecord:
		// json[b]_to_record (R1): one record row, mapping members → the col-def columns by name.
		if err := m.Guard(); err != nil {
			return nil, err
		}
		m.Charge(costs.GeneratedRow)
		row, err := jsonRecordRow(&node, sp.recordCols, env, m)
		if err != nil {
			return nil, err
		}
		out = append(out, row)
	case srfJSONRecordset:
		// json[b]_to_recordset (R1): one record row per element of a top-level array (preserving
		// order); a non-array document → 22023.
		if node.Kind != JArray {
			return nil, newError(InvalidParameterValue, "cannot call json_to_recordset on a non-array")
		}
		for i := range node.Arr {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			row, err := jsonRecordRow(&node.Arr[i], sp.recordCols, env, m)
			if err != nil {
				return nil, err
			}
			out = append(out, row)
		}
	case srfJsonbPathQuery:
		// jsonb_path_query (P2, jsonpath.md §5.2): one jsonb row per path-evaluation-sequence item.
		// The context node is already parsed above (`node`); evaluate the path (a NULL path → zero
		// rows). The resolver restricts the path argument to jsonpath (its canonical text in Str).
		path, err := sp.args[1].eval(nil, env, m)
		if err != nil {
			return nil, err
		}
		if path.Kind == ValNull {
			return nil, nil
		}
		compiled, err := compile(path.str())
		if err != nil {
			return nil, err
		}
		seq, err := compiled.Eval(node)
		if err != nil {
			return nil, err
		}
		for i := range seq {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			out = append(out, storedRow{JsonbValue(seq[i])})
		}
	default:
		panic("jsonSrfRows only handles the json SRF kinds")
	}
	return out, nil
}

// jsonRecordRow builds one output row for json[b]_to_record(set) (R1): map each declared column to
// the JSON object's member of that name, coercing it to the column type. A missing member or a JSON
// null → SQL NULL; a non-object node → 22023. (json-table.md §2)
func jsonRecordRow(node *JsonNode, cols []catColumn, env *evalEnv, m *costMeter) (storedRow, error) {
	if node.Kind != JObject {
		return nil, newError(InvalidParameterValue, "argument of json_to_record must be a JSON object")
	}
	row := make(storedRow, 0, len(cols))
	for ci := range cols {
		col := &cols[ci]
		var member *JsonNode
		for mi := range node.Obj {
			if node.Obj[mi].Key == col.Name {
				member = &node.Obj[mi].Val
				break
			}
		}
		// A missing member or a JSON null member → SQL NULL.
		if member == nil || member.Kind == JNull {
			row = append(row, NullValue())
			continue
		}
		v, err := coerceJSONMember(member, col.Type, col.Decimal, env, m)
		if err != nil {
			return nil, err
		}
		row = append(row, v)
	}
	return row, nil
}

// coerceJSONMember coerces a JSON member node to a record column's type (R1, the JSON_VALUE scalar
// path): a `jsonb` column embeds the node, a `json` column its canonical text, every other scalar
// coerces the node's `->>`-style text through the cast machinery (so `"42"` / `42` → an `int`
// column, etc.). A composite/array column type is a deferred 0A000.
func coerceJSONMember(node *JsonNode, colTy dataType, decimal *decimalTypmod, env *evalEnv, m *costMeter) (Value, error) {
	// A composite / array / range field type is a deferred 0A000 (only scalar / json / jsonb coerce
	// this slice). R1's col-def list rejects these at resolve; R2's composite fields can carry one.
	if _, ok := colTy.AsScalar(); !ok {
		return Value{}, newError(FeatureNotSupported, "a composite/array record column is not supported yet")
	}
	st := colTy.ScalarTy()
	switch {
	case st == scalarJsonb:
		return JsonbValue(*node), nil
	case st == scalarJson:
		return JsonValue(jsonbOut(node)), nil
	default:
		text, ok := jsonNodeToText(node)
		if !ok {
			return NullValue(), nil
		}
		rexpr, _, err := coerceStringLiteral(text, st, decimal, nil)
		if err != nil {
			return Value{}, err
		}
		return rexpr.eval(nil, env, m)
	}
}

// isSQLJSONError reports whether an error is a SQL/JSON error caught by a query function's `ON ERROR`
// clause: a data exception (class `22`). Resource / cost aborts (class `53`/`54`) propagate
// unconditionally.
func isSQLJSONError(err error) bool {
	if ee, ok := err.(*EngineError); ok {
		return strings.HasPrefix(ee.Code(), "22")
	}
	return false
}

// applyJSONBehavior applies a constant `ON ERROR` / `ON EMPTY` behavior → a value of the RETURNING
// type. underlying is the SQL/JSON error this behavior replaces (raised verbatim by `ERROR`).
func applyJSONBehavior(behavior jsonOnBehavior, underlying error, returning scalarType, env *evalEnv, m *costMeter) (Value, error) {
	switch behavior {
	case jOBError:
		return Value{}, underlying
	case jOBNull:
		return NullValue(), nil
	case jOBTrue:
		return BoolValue(true), nil
	case jOBFalse:
		return BoolValue(false), nil
	case jOBUnknown:
		return NullValue(), nil
	case jOBEmptyArray:
		return jsonNodeAsReturning(JsonNode{Kind: JArray}, returning, env, m)
	default: // JOBEmptyObject
		return jsonNodeAsReturning(JsonNode{Kind: JObject}, returning, env, m)
	}
}

// jsonNodeAsReturning renders a json result node as the RETURNING type: `jsonb` embeds, `json` its
// canonical text, any other scalar coerces the node's `->>`-style text through the cast machinery.
func jsonNodeAsReturning(node JsonNode, returning scalarType, env *evalEnv, m *costMeter) (Value, error) {
	return coerceJSONMember(&node, scalarT(returning), nil, env, m)
}

// evalJSONSqlResult applies the SQL/JSON query-function semantics (JSON_VALUE / JSON_QUERY) to an
// evaluated sequence. (JSON_EXISTS is handled inline — non-empty → true.)
func evalJSONSqlResult(kind jsonSqlKind, seq []JsonNode, returning scalarType, wrapper jsonWrapper, onEmpty, onError jsonOnBehavior, env *evalEnv, m *costMeter) (Value, error) {
	switch kind {
	case jsExists:
		return BoolValue(len(seq) > 0), nil
	case jsValue:
		if len(seq) == 0 {
			return applyJSONBehavior(onEmpty, newError(NoSqlJsonItem, "no SQL/JSON item"), returning, env, m)
		}
		if len(seq) > 1 {
			return applyJSONBehavior(onError,
				newError(MoreThanOneSqlJsonItem, "JSON path expression in JSON_VALUE should return singleton scalar item"),
				returning, env, m)
		}
		item := seq[0]
		// JSON_VALUE requires a SCALAR item (PG 2203F otherwise).
		if item.Kind == JArray || item.Kind == JObject {
			return applyJSONBehavior(onError,
				newError(SqlJsonMemberNotFound, "JSON path expression in JSON_VALUE should return singleton scalar item"),
				returning, env, m)
		}
		// Coerce the scalar to the RETURNING type (a JSON null → SQL NULL). A coercion failure is a
		// SQL/JSON error honored by ON ERROR.
		v, err := coerceJSONMember(&item, scalarT(returning), nil, env, m)
		if err != nil {
			if isSQLJSONError(err) {
				return applyJSONBehavior(onError, err, returning, env, m)
			}
			return Value{}, err
		}
		return v, nil
	default: // jsQuery
		var node JsonNode
		switch wrapper {
		case jWUnconditional:
			node = JsonNode{Kind: JArray, Arr: seq}
		case jWConditional:
			if len(seq) == 1 {
				node = seq[0]
			} else {
				node = JsonNode{Kind: JArray, Arr: seq}
			}
		default: // JWWithout
			if len(seq) == 0 {
				return applyJSONBehavior(onEmpty, newError(NoSqlJsonItem, "no SQL/JSON item"), returning, env, m)
			}
			if len(seq) > 1 {
				return applyJSONBehavior(onError,
					newError(MoreThanOneSqlJsonItem, "JSON path expression in JSON_QUERY should return singleton item without wrapper"),
					returning, env, m)
			}
			node = seq[0]
		}
		return jsonNodeAsReturning(node, returning, env, m)
	}
}

// ----------------------------------------------------------------------------------------------
// JSON_TABLE (T1, json-table.md §3)
// ----------------------------------------------------------------------------------------------

// jtAssign is a sparse assignment of a JSON_TABLE row — `(flat column index, value)` pairs;
// unassigned columns are NULL (the LEFT-OUTER / sibling-UNION fill).
type jtAssign struct {
	idx int
	v   Value
}

// jsonTableRows generates the rows of a JSON_TABLE SRF (T1, json-table.md §3) — the default-plan
// recursive expansion (parent→child LEFT OUTER, sibling NESTED paths UNIONed). A NULL ctx → zero
// rows; a structural error evaluating the root path → zero rows.
func (db *engine) jsonTableRows(sp *srfPlan, env *evalEnv, m *costMeter) ([]storedRow, error) {
	plan := sp.jsonTable
	ctx, err := sp.args[0].eval(nil, env, m)
	if err != nil {
		return nil, err
	}
	if ctx.Kind == ValNull {
		return nil, nil
	}
	node, err := jsonArgNode(ctx)
	if err != nil {
		return nil, err
	}
	// The root path → the sequence of row items (a structural error here yields no rows).
	root, err := compile(plan.rootPath)
	if err != nil {
		return nil, err
	}
	items, err := root.Eval(node)
	if err != nil {
		if isSQLJSONError(err) {
			return nil, nil
		}
		return nil, err
	}
	// Expand the column tree over the root sequence → sparse rows, then materialize.
	sparse, err := expandJtLevel(plan.columns, items, env, m)
	if err != nil {
		return nil, err
	}
	out := make([]storedRow, 0, len(sparse))
	for _, assignment := range sparse {
		if err := m.Guard(); err != nil {
			return nil, err
		}
		m.Charge(costs.GeneratedRow)
		row := make(storedRow, plan.width)
		for i := range row {
			row[i] = NullValue()
		}
		for _, a := range assignment {
			row[a.idx] = a.v
		}
		out = append(out, row)
	}
	return out, nil
}

// jtColumn builds a synthetic JSON_TABLE output column.
func newJtColumn(name string, ty scalarType, decimal *decimalTypmod) catColumn {
	return catColumn{Name: name, Type: scalarT(ty), Decimal: decimal}
}

// jtBehavior resolves an optional ON EMPTY / ON ERROR behavior to its value, falling back to def.
func jtBehavior(b *jsonOnBehavior, def jsonOnBehavior) jsonOnBehavior {
	if b != nil {
		return *b
	}
	return def
}

// jtScalarType resolves a JSON_TABLE column type name → its scalar type + decimal typmod (a composite
// → 0A000, an unknown name → 42704).
func jtScalarType(db *engine, typeName string) (scalarType, *decimalTypmod, error) {
	if st, ok := scalarTypeFromName(typeName); ok {
		return st, nil, nil
	}
	if db.CompositeType(typeName) != nil {
		return 0, nil, newError(FeatureNotSupported, "a composite JSON_TABLE column is not supported yet")
	}
	return 0, nil, newError(UndefinedObject, fmt.Sprintf("type \"%s\" does not exist", typeName))
}

// jtCompilePath compiles a JSON_TABLE column path — the explicit `PATH p`, or the default
// `$.<column_name>` — to its canonical rendered form (validating; malformed → 42601).
func jtCompilePath(path *string, name string) (string, error) {
	src := "$." + name
	if path != nil {
		src = *path
	}
	compiled, err := compile(src)
	if err != nil {
		return "", err
	}
	return compiled.Render(), nil
}

// expandJtLevel expands a JSON_TABLE COLUMNS level over a sequence of row items → the sparse rows
// (the parent→child LEFT OUTER product with sibling NESTED paths UNIONed, json-table.md §3.3).
func expandJtLevel(cols []jtCol, items []JsonNode, env *evalEnv, m *costMeter) ([][]jtAssign, error) {
	var rows [][]jtAssign
	for i := range items {
		if err := m.Guard(); err != nil {
			return nil, err
		}
		ord := int64(i + 1)
		item := &items[i]
		// This level's non-nested columns (regular / exists / ordinality).
		var local []jtAssign
		for _, col := range cols {
			switch c := col.(type) {
			case *jtColOrdinality:
				local = append(local, jtAssign{idx: c.idx, v: IntValue(ord)})
			case *jtColRegular:
				v, err := evalJtRegular(item, c, env, m)
				if err != nil {
					return nil, err
				}
				local = append(local, jtAssign{idx: c.idx, v: v})
			case *jtColExists:
				v, err := evalJtExists(item, c)
				if err != nil {
					return nil, err
				}
				local = append(local, jtAssign{idx: c.idx, v: v})
			case *jtColNested:
				// handled below
			}
		}
		// The NESTED siblings, expanded over this item (UNIONed + LEFT OUTER fill).
		var nested []*jtColNested
		for _, col := range cols {
			if n, ok := col.(*jtColNested); ok {
				nested = append(nested, n)
			}
		}
		nestedRows, err := expandJtNested(nested, item, env, m)
		if err != nil {
			return nil, err
		}
		for _, nr := range nestedRows {
			row := make([]jtAssign, 0, len(local)+len(nr))
			row = append(row, local...)
			row = append(row, nr...)
			rows = append(rows, row)
		}
	}
	return rows, nil
}

// expandJtNested expands the NESTED siblings of a level over one parent item — the default-plan
// UNION of the siblings (each row fills only its own subtree), with the parent→child LEFT OUTER fill
// (no child rows at all → one all-NULL nested row).
func expandJtNested(children []*jtColNested, item *JsonNode, env *evalEnv, m *costMeter) ([][]jtAssign, error) {
	if len(children) == 0 {
		return [][]jtAssign{nil}, nil
	}
	var union [][]jtAssign
	for _, child := range children {
		p, err := compile(child.path)
		if err != nil {
			return nil, err
		}
		childSeq, err := p.Eval(*item)
		if err != nil {
			if isSQLJSONError(err) {
				childSeq = nil
			} else {
				return nil, err
			}
		}
		rows, err := expandJtLevel(child.columns, childSeq, env, m)
		if err != nil {
			return nil, err
		}
		union = append(union, rows...)
	}
	if len(union) == 0 {
		union = append(union, nil)
	}
	return union, nil
}

// evalJtRegular evaluates a regular JSON_TABLE column over a row item — JSON_VALUE (scalar) /
// JSON_QUERY (json/jsonb) semantics, with the column's wrapper / ON EMPTY / ON ERROR.
func evalJtRegular(item *JsonNode, c *jtColRegular, env *evalEnv, m *costMeter) (Value, error) {
	p, err := compile(c.path)
	if err != nil {
		return Value{}, err
	}
	seq, err := p.Eval(*item)
	if err != nil {
		if isSQLJSONError(err) {
			return applyJSONBehavior(c.onError, err, c.returning, env, m)
		}
		return Value{}, err
	}
	kind := jsValue
	if c.query {
		kind = jsQuery
	}
	return evalJSONSqlResult(kind, seq, c.returning, c.wrapper, c.onEmpty, c.onError, env, m)
}

// evalJtExists evaluates an EXISTS JSON_TABLE column over a row item — JSON_EXISTS, coerced to the
// column type (a NON-empty sequence is true; a structural error honors ON ERROR, default FALSE).
func evalJtExists(item *JsonNode, c *jtColExists) (Value, error) {
	p, err := compile(c.path)
	if err != nil {
		return Value{}, err
	}
	var exists bool
	seq, err := p.Eval(*item)
	if err != nil {
		if isSQLJSONError(err) {
			switch c.onError {
			case jOBError:
				return Value{}, err
			case jOBTrue:
				exists = true
			case jOBUnknown:
				return NullValue(), nil
			default:
				exists = false
			}
		} else {
			return Value{}, err
		}
	} else {
		exists = len(seq) > 0
	}
	// Coerce the boolean to the column type (a `boolean` column → bool; an integer column → 1/0).
	switch {
	case c.returning.IsBool():
		return BoolValue(exists), nil
	case c.returning.IsInteger():
		if exists {
			return IntValue(1), nil
		}
		return IntValue(0), nil
	default:
		return Value{}, newError(FeatureNotSupported, "an EXISTS JSON_TABLE column must be boolean or integer this slice")
	}
}

// unnestRows generates the rows of an unnest(anyarray) FROM-clause source (spec/design/array-functions.md
// §9), as one-column rows. The single array argument evaluates ONCE against the outer environment with
// no local row (non-LATERAL). PostgreSQL semantics: a NULL array yields zero rows; the empty array {}
// yields zero rows; otherwise one row per element in flattened row-major order (a multidimensional array
// flattens; a NULL element is produced as a NULL row). Each produced element charges one generated_row AT
// THE SOURCE, guarded so a max_cost ceiling aborts a runaway unnest (54P01) mid-generation, exactly like
// generate_series (CLAUDE.md §13).
func (db *engine) unnestRows(sp *srfPlan, env *evalEnv, m *costMeter) ([]storedRow, error) {
	v, err := sp.args[0].eval(nil, env, m)
	if err != nil {
		return nil, err
	}
	switch v.Kind {
	case ValNull:
		// A NULL array → zero rows (PG; the empty_on_null discipline).
		return nil, nil
	case ValArray:
		out := make([]storedRow, 0, len(v.arrayVal().Elements))
		for _, e := range v.arrayVal().Elements {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			out = append(out, storedRow{e})
		}
		return out, nil
	default:
		panic("the resolver restricts unnest's argument to an array")
	}
}
