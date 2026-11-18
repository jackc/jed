package jed

import (
	"fmt"
	"slices"
	"strconv"
)

// Stored generated columns (spec/design/generated-columns.md): admitting a generation expression at
// CREATE TABLE / ADD COLUMN (§2), resolving the table's generation expressions for a write
// statement, and computing them over a built row (§3.1 step 2).

// requireStored is the 0A000 for a generation clause that is not STORED (generated-columns.md §1).
func requireStored(def *generatedDef) error {
	if def.Kind == generatedStored {
		return nil
	}
	return newError(FeatureNotSupported, "virtual generated columns are not supported")
}

// generatedExprOf is the catalog form of a parsed generation clause (its expression and persisted
// text), or nil for no clause.
func generatedExprOf(def *generatedDef) *defaultExprDef {
	if def == nil {
		return nil
	}
	return &defaultExprDef{ExprText: def.Text, Expr: def.Expr}
}

// rejectGeneratedStructure applies the structural rejections for a generation expression
// (generated-columns.md §2 step 1) in one depth-first pre-walk before resolution, as for a CHECK: a
// subquery or set-returning function is 0A000, an aggregate 42803, a window call 42P20, a bind
// parameter 42P02.
func rejectGeneratedStructure(e exprNode) error {
	switch e.Kind {
	case exprScalarSubquery, exprExists, exprInSubquery, exprQuantifiedSubquery:
		return newError(FeatureNotSupported, "cannot use subquery in column generation expression")
	case exprParam:
		return newError(UndefinedParameter, "there is no parameter $"+strconv.FormatUint(e.Param, 10))
	case exprFuncCall:
		fc := e.FuncCall
		if fc.Over != nil || fc.OverName != "" {
			return newError(WindowingError, "window functions are not allowed in column generation expressions")
		}
		if isAggregateName(fc.Name) {
			return newError(GroupingError, "aggregate functions are not allowed in column generation expressions")
		}
		if isSetReturningName(fc.Name) {
			return newError(FeatureNotSupported, "set-returning functions are not allowed in column generation expressions")
		}
	}
	for _, c := range generatedExprChildren(&e) {
		if err := rejectGeneratedStructure(*c); err != nil {
			return err
		}
	}
	return nil
}

// generatedExprChildren is the direct sub-expressions of e a row-level walk descends into (a
// subquery's body is not one: it is rejected before its children matter).
func generatedExprChildren(e *exprNode) []*exprNode {
	var out []*exprNode
	add := func(xs ...*exprNode) { out = append(out, xs...) }
	addList := func(xs []exprNode) {
		for i := range xs {
			out = append(out, &xs[i])
		}
	}
	switch e.Kind {
	case exprRow, exprArray:
		addList(e.RowItems)
	case exprCoalesce:
		addList(e.Coalesce)
	case exprGreatestLeast:
		addList(e.GreatestLeast)
	case exprNullIf:
		addList(e.NullIf)
	case exprFieldAccess, exprFieldStar:
		add(e.Base)
	case exprSubscript:
		add(e.Base)
		for _, s := range e.Subscripts {
			add(subscriptSpecExprs(s)...)
		}
	case exprCast:
		add(&e.Cast.Inner)
	case exprCollate:
		add(&e.Collate.Inner)
	case exprExtract:
		add(&e.Extract.Source)
	case exprUnary:
		add(&e.Unary.Operand)
	case exprIsNull:
		add(&e.IsNullOf.Operand)
	case exprIsJson:
		add(&e.IsJsonOf.Operand)
	case exprJsonCtor:
		add(&e.JsonCtorOf.Operand)
	case exprJsonExists:
		add(&e.JsonExists.Ctx, &e.JsonExists.Path)
		for i := range e.JsonExists.Passing {
			add(&e.JsonExists.Passing[i].Expr)
		}
	case exprJsonValue:
		add(&e.JSONValue.Ctx, &e.JSONValue.Path)
		for i := range e.JSONValue.Passing {
			add(&e.JSONValue.Passing[i].Expr)
		}
	case exprJsonQuery:
		add(&e.JsonQuery.Ctx, &e.JsonQuery.Path)
		for i := range e.JsonQuery.Passing {
			add(&e.JsonQuery.Passing[i].Expr)
		}
	case exprBinary:
		add(&e.Binary.Lhs, &e.Binary.Rhs)
	case exprIsDistinct:
		add(&e.IsDistinct.Lhs, &e.IsDistinct.Rhs)
	case exprLike:
		add(&e.Like.Lhs, &e.Like.Rhs)
	case exprRegex:
		add(&e.Regex.Lhs, &e.Regex.Rhs)
	case exprIn:
		add(&e.In.Lhs)
		addList(e.In.List)
	case exprBetween:
		add(&e.Between.Lhs, &e.Between.Lo, &e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil {
			add(e.Case.Operand)
		}
		for i := range e.Case.Whens {
			add(&e.Case.Whens[i].Cond, &e.Case.Whens[i].Result)
		}
		if e.Case.Els != nil {
			add(e.Case.Els)
		}
	case exprFuncCall:
		add(e.FuncCall.Args...)
		if e.FuncCall.Filter != nil {
			add(e.FuncCall.Filter)
		}
	case exprInSubquery:
		add(&e.InSubquery.Lhs)
	case exprQuantifiedSubquery:
		add(&e.QuantifiedSubquery.Lhs)
	case exprQuantified:
		add(&e.Quantified.Lhs, &e.Quantified.Array)
	}
	return out
}

// isSetReturningName reports the set-returning built-ins a scalar call position can name
// (functions.md §10). In a generation expression each is 0A000, PostgreSQL's rejection.
func isSetReturningName(name string) bool {
	switch name {
	case "generate_series", "generate_subscripts", "unnest", "regexp_matches",
		"regexp_split_to_table", "string_to_table", "json_array_elements",
		"json_array_elements_text", "jsonb_array_elements", "jsonb_array_elements_text",
		"json_each", "json_each_text", "jsonb_each", "jsonb_each_text", "json_object_keys",
		"jsonb_object_keys", "jsonb_path_query", "json_populate_recordset",
		"jsonb_populate_recordset", "json_to_recordset", "jsonb_to_recordset":
		return true
	}
	return false
}

// rexprCallsHost reports whether a resolved expression calls a host function anywhere.
func rexprCallsHost(node *rExpr) bool {
	found := false
	_ = walkIndexExprHostFuncs(node, func(*rExpr) error {
		found = true
		return nil
	})
	return found
}

// admitGeneratedExpr admits column ci's generation expression against table
// (generated-columns.md §2), in PostgreSQL's order: the structural pre-walk; name and type
// resolution (42703, operator errors); a reference to a generated column (42P17); immutability
// (42P17); then the host-function and timezone narrowings (0A000) and the assignment type (42804).
// table holds every column of the statement's end shape, so a later column is in scope.
func (db *engine) admitGeneratedExpr(table *catTable, ci int) error {
	col := &table.Columns[ci]
	ge := col.Generated
	if err := rejectGeneratedStructure(ge.Expr); err != nil {
		return err
	}
	s := singleScope(db, table)
	pt := &paramTypes{indexContext: true}
	var node *rExpr
	var rt resolvedType
	scalar := col.Type.isScalar()
	if scalar {
		sty := col.Type.ScalarTy()
		n, r, err := resolve(s, ge.Expr, &sty, &aggCtx{collecting: false}, pt)
		if err != nil {
			return err
		}
		node, rt = n, r
	} else {
		n, err := resolveContainerAssignTy(s, col.Type, col.Name, ge.Expr, &aggCtx{collecting: false}, pt)
		if err != nil {
			return err
		}
		node = n
	}
	for _, r := range checkReferencedColumns(ge.Expr, table.Columns) {
		if table.Columns[r].Generated != nil {
			return newError(InvalidObjectDefinition, fmt.Sprintf(
				"cannot use generated column %s in column generation expression", table.Columns[r].Name,
			)).withDetail("A generated column cannot reference another generated column.")
		}
	}
	if pt.nonimmutable {
		return newError(InvalidObjectDefinition, "generation expression is not immutable")
	}
	if rexprCallsHost(node) {
		return newError(FeatureNotSupported, "a host function in a column generation expression is not supported yet")
	}
	if !pt.timezoneDeps.empty() {
		return newError(FeatureNotSupported, "a time zone dependency in a column generation expression is not supported yet")
	}
	if scalar {
		sty := col.Type.ScalarTy()
		if !assignableTo(rt, sty) {
			return assignmentError(fmt.Sprintf(
				"column %s is of type %s but default expression is of type %s",
				col.Name, sty.CanonicalName(), rtName(rt),
			))
		}
	}
	return nil
}

// resolveGeneratedExprs resolves the generation expressions a write statement computes, one slot
// per column (nil for an ordinary column, or a generated one need leaves out). Each resolves
// against the table's own columns, as admitted. An unparsable stored expression is XX003
// (generated-columns.md §6) — only for a column the statement computes.
func (db *engine) resolveGeneratedExprs(table *catTable, need []bool, ptypes *paramTypes) ([]*rExpr, error) {
	out := make([]*rExpr, len(table.Columns))
	s := singleScope(db, table)
	for i := range table.Columns {
		col := &table.Columns[i]
		ge := col.Generated
		if ge == nil || (need != nil && !need[i]) {
			continue
		}
		if ge.Unparsed {
			return nil, unparsableGeneratedError(table.Name, col.Name)
		}
		if col.Type.isScalar() {
			sty := col.Type.ScalarTy()
			node, _, err := resolve(s, ge.Expr, &sty, &aggCtx{collecting: false}, ptypes)
			if err != nil {
				return nil, err
			}
			out[i] = node
			continue
		}
		node, err := resolveContainerAssignTy(s, col.Type, col.Name, ge.Expr, &aggCtx{collecting: false}, ptypes)
		if err != nil {
			return nil, err
		}
		out[i] = node
	}
	return out, nil
}

// computeGenerated computes each generated column's resolved expression (generated[i], read only
// where column i is generated — a caller may pass the INSERT default slots) over row in column
// order, coercing each result to its column's type and typmod (generated-columns.md §3.1 step 2).
// row holds the built base row; a generated slot is overwritten. NOT NULL is the caller's (it runs
// over the complete row afterwards), so a NULL result is stored as is here.
func (db *engine) computeGenerated(columns []catColumn, colTypes []colType, generated []*rExpr, row storedRow, rng *stmtRng, meter *costMeter, relation string) error {
	any := false
	for i := range generated {
		if generated[i] != nil && columns[i].Generated != nil {
			any = true
			break
		}
	}
	if !any {
		return nil
	}
	if err := meter.Checkpoint(); err != nil {
		return err
	}
	env := &evalEnv{exec: db, rng: rng}
	for i, g := range generated {
		col := &columns[i]
		if g == nil || col.Generated == nil {
			continue
		}
		v, err := g.eval(row, env, meter)
		if err != nil {
			return err
		}
		if v.kind == ValNull {
			row[i] = NullValue()
			continue
		}
		cv, err := coerceForStore(v, colTypes[i], columnTypmod(col), false, col.Name, db.inputZone())
		if err != nil {
			return assignmentHint(stampTable(err, relation))
		}
		row[i] = cv
	}
	return nil
}

// generatedNotNull folds the NOT NULL violations of the computed generated columns into nullCol
// (the smallest failing column index wins, generated-columns.md §3.1 step 3): only the columns
// generated marks are considered. Returns the updated index (-1 for none).
func generatedNotNull(columns []catColumn, generated []*rExpr, row storedRow, nullCol int) int {
	for i, g := range generated {
		if g != nil && columns[i].Generated != nil && columns[i].NotNull && row[i].kind == ValNull && (nullCol < 0 || i < nullCol) {
			nullCol = i
		}
	}
	return nullCol
}

// generatedRecomputeSet is which generated columns an UPDATE recomputes (generated-columns.md
// §3.2): exactly those whose expression references one of the assigned columns. assigned[i] marks
// column i a SET target. Returns nil when the table has no generated column the statement touches.
func generatedRecomputeSet(table *catTable, assigned []bool) []bool {
	var need []bool
	for i := range table.Columns {
		ge := table.Columns[i].Generated
		if ge == nil {
			continue
		}
		for _, r := range checkReferencedColumns(ge.Expr, table.Columns) {
			if assigned[r] {
				if need == nil {
					need = make([]bool, len(table.Columns))
				}
				need[i] = true
				break
			}
		}
	}
	return need
}

// generatedInsertError is the 428C9 for an INSERT supplying a non-DEFAULT value to a generated
// column.
func generatedInsertError(column string) *EngineError {
	return newError(GeneratedAlways, "cannot insert a non-DEFAULT value into column "+column).
		withDetail("Column \"" + column + "\" is a generated column.").
		noHint()
}

// generatedUpdateError is the 428C9 for an UPDATE assigning a non-DEFAULT value to a generated
// column.
func generatedUpdateError(column string) *EngineError {
	return newError(GeneratedAlways, "column "+column+" can only be updated to DEFAULT").
		withDetail("Column \"" + column + "\" is a generated column.").
		noHint()
}

// checkFkGeneratedActions rejects a foreign key whose referencing columns include a generated column
// with an action that writes them (generated-columns.md §2, "Foreign keys"): ON UPDATE CASCADE /
// SET NULL / SET DEFAULT and ON DELETE SET NULL / SET DEFAULT are 42601, PostgreSQL's rule.
func checkFkGeneratedActions(table *catTable, local []int, onDelete, onUpdate fkAction) error {
	if !slices.ContainsFunc(local, func(i int) bool { return table.Columns[i].Generated != nil }) {
		return nil
	}
	if onUpdate == fkCascade || onUpdate == fkSetNull || onUpdate == fkSetDefault {
		return newError(SyntaxError, "invalid ON UPDATE action for foreign key constraint containing generated column")
	}
	if onDelete == fkSetNull || onDelete == fkSetDefault {
		return newError(SyntaxError, "invalid ON DELETE action for foreign key constraint containing generated column")
	}
	return nil
}

// generatedReaderOf is the first generated column (in column order) whose expression references
// column ci, or -1.
func generatedReaderOf(table *catTable, ci int) int {
	for i := range table.Columns {
		if g := table.Columns[i].Generated; g != nil && slices.Contains(checkReferencedColumns(g.Expr, table.Columns), ci) {
			return i
		}
	}
	return -1
}

// insertTargetedError is the 428C9 for an INSERT target column that only takes DEFAULT: a stored
// generated column (generated-columns.md §3.1) or a GENERATED ALWAYS identity column
// (sequences.md §13.3).
func insertTargetedError(col *catColumn) *EngineError {
	if col.Generated != nil {
		return generatedInsertError(col.Name)
	}
	return newGeneratedAlwaysInsert(col.Name)
}
