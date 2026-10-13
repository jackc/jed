package jed

// A recursive CTE's SEARCH / CYCLE clauses (spec/design/recursive-cte.md §9).
//
// The clauses add columns to the CTE: SEARCH an ordering column, CYCLE a cycle mark and a path.
// PostgreSQL defines them by a rewrite of the CTE (rewriteSearchCycle.c); jed computes the same
// columns from each term's produced rows. The planner resolves two lists of expressions over a
// synthetic row — one over an anchor row (the user columns), one over a recursive-term row (the user
// columns, then the parent row's added columns, which the recursive term carries as hidden trailing
// outputs) — plus CYCLE's filter, and the executor evaluates them per row.

import (
	"fmt"
	"slices"
)

// searchCyclePlan is the planned SEARCH / CYCLE columns of a recursive CTE (recursive-cte.md §9).
// Every expression is resolved against a synthetic row: anchor over an anchor row's userCols values;
// filter and recursive over a recursive-term row — its userCols values (coerced to the anchor's
// types) followed by the parent row's added columns.
type searchCyclePlan struct {
	// userCols is the CTE's own column count; the added columns follow them.
	userCols int
	// names / types are the added columns', in order: the search column, then the cycle mark and
	// the cycle path.
	names []string
	types []resolvedType
	// anchor gives the added columns' values for an anchor row, recursive for a recursive-term row.
	anchor    []*rExpr
	recursive []*rExpr
	// filter is CYCLE's `mark <> value` over the parent's mark: a recursive-term row is kept only
	// when it is TRUE, so a row whose parent closed a cycle is not expanded. nil without CYCLE.
	filter *rExpr
}

// cycleMark is a CYCLE clause's mark constants, typed (recursive-cte.md §9): value marks a row that
// closes a cycle, def every other row. Both have the common type ty.
type cycleMark struct {
	value, def exprNode
	ty, defTy  resolvedType
}

// scUserCol is the synthetic column of user column i in an augment row. Never visible: the augment
// expressions build anonymous rows, whose fields are f1…fN whatever the columns are called.
func scUserCol(i int) exprNode {
	return exprNode{Kind: exprColumn, Column: fmt.Sprintf("*%d*", i)}
}

// scAddedCol is the synthetic column of added column k (search, mark, path) in a recursive row.
func scAddedCol(k int) exprNode {
	return exprNode{Kind: exprColumn, Column: fmt.Sprintf("*added%d*", k)}
}

func scRowOf(cols []int) exprNode {
	items := make([]exprNode, len(cols))
	for j, i := range cols {
		items[j] = scUserCol(i)
	}
	return exprNode{Kind: exprRow, RowItems: items}
}

func scIntLit(n int64) exprNode {
	return exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalInt, Int: n}}
}

func scBinary(op binaryOp, lhs, rhs exprNode) exprNode {
	return exprNode{Kind: exprBinary, Binary: &binaryExpr{Op: op, Lhs: lhs, Rhs: rhs}}
}

func scArrayOf(e exprNode) exprNode {
	return exprNode{Kind: exprArray, RowItems: []exprNode{e}}
}

// resolveCycleMark types a CYCLE clause's mark constants (recursive-cte.md §9), before the CTE's body
// is planned (PostgreSQL's order). Omitted, they are TRUE / FALSE. A bare string literal takes the
// other constant's type, read through that type's input (`TO 1 DEFAULT 'N'` is 22P02, as in
// PostgreSQL); the two must then have a common type (42804), and that type an equality operator
// (42883).
func (db *engine) resolveCycleMark(c *cycleClause, ptypes *paramTypes) (*cycleMark, error) {
	var value, def exprNode
	if c.MarkValue != nil && c.MarkDefault != nil {
		value, def = *c.MarkValue, *c.MarkDefault
	} else {
		value = exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalBool, Bool: true}}
		def = exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalBool, Bool: false}}
	}
	empty, err := cteSyntheticTableCols("*cycle*", nil, nil, nil, nil, nil)
	if err != nil {
		return nil, err
	}
	s := singleScope(db, empty)
	typeOf := func(e exprNode) (resolvedType, error) {
		_, t, err := resolve(s, e, nil, &aggCtx{}, ptypes)
		return t, err
	}
	vt, err := typeOf(value)
	if err != nil {
		return nil, err
	}
	dt, err := typeOf(def)
	if err != nil {
		return nil, err
	}
	// A bare string literal is PostgreSQL's unknown: it takes the other constant's type.
	adapt := func(e *exprNode, other resolvedType) {
		if e.Kind != exprLiteral || e.Literal == nil || e.Literal.Kind != literalText {
			return
		}
		if st, ok := elemScalarHint(other); ok && !st.IsText() {
			*e = exprNode{Kind: exprTypedLiteral, TypeLitName: st.CanonicalName(), TypeLitText: e.Literal.Str, Pos: e.Pos}
		}
	}
	adapt(&value, dt)
	adapt(&def, vt)
	if vt, err = typeOf(value); err != nil {
		return nil, err
	}
	if dt, err = typeOf(def); err != nil {
		return nil, err
	}
	var ty resolvedType
	switch {
	case vt.kind == rtNull && dt.kind == rtNull:
		ty = resolvedType{kind: rtText}
	case vt.kind == rtNull:
		ty = dt
	case dt.kind == rtNull:
		ty = vt
	default:
		t, ok := commonType(vt, dt)
		if !ok {
			return nil, newError(DatatypeMismatch, fmt.Sprintf(
				"CYCLE types %s and %s cannot be matched", rtName(vt), rtName(dt),
			)).withPosition(int32(c.Pos))
		}
		ty = t
	}
	if err := requireSetopEquality(ty); err != nil {
		return nil, err
	}
	return &cycleMark{value: value, def: def, ty: ty, defTy: dt}, nil
}

// planSearchCycle validates a recursive CTE's SEARCH / CYCLE clauses against its columns and resolves
// the added columns' expressions (recursive-cte.md §9). user is the CTE's relation (its columns named
// as the CTE names them) and userTypes their resolved types.
func (db *engine) planSearchCycle(c *cte, user *catTable, userTypes []resolvedType, mark *cycleMark, ptypes *paramTypes) (*searchCyclePlan, error) {
	names := make([]string, len(user.Columns))
	for i := range user.Columns {
		names[i] = user.Columns[i].Name
	}
	var searchCols, cycleCols []int
	if s := c.Search; s != nil {
		cols, err := scClauseColumns(s.Columns, names, "search", s.Pos)
		if err != nil {
			return nil, err
		}
		searchCols = cols
		if slices.Contains(names, s.SetColumn) {
			return nil, scClauseError(fmt.Sprintf(
				"search sequence column name \"%s\" already used in WITH query column list", s.SetColumn), s.Pos)
		}
	}
	if cy := c.Cycle; cy != nil {
		cols, err := scClauseColumns(cy.Columns, names, "cycle", cy.Pos)
		if err != nil {
			return nil, err
		}
		cycleCols = cols
		for _, pair := range [][2]string{{cy.MarkColumn, "mark"}, {cy.PathColumn, "path"}} {
			if slices.Contains(names, pair[0]) {
				return nil, scClauseError(fmt.Sprintf(
					"cycle %s column name \"%s\" already used in WITH query column list", pair[1], pair[0]), cy.Pos)
			}
		}
		if cy.MarkColumn == cy.PathColumn {
			return nil, scClauseError("cycle mark column name and cycle path column name are the same", cy.Pos)
		}
	}
	if s, cy := c.Search, c.Cycle; s != nil && cy != nil {
		for _, pair := range [][2]string{{cy.MarkColumn, "mark"}, {cy.PathColumn, "path"}} {
			if s.SetColumn == pair[0] {
				return nil, scClauseError(fmt.Sprintf(
					"search sequence column name and cycle %s column name are the same", pair[1]), s.Pos)
			}
		}
	}

	// The added columns' expressions, over the synthetic anchor row (`*i*` = user column i) and
	// recursive row (then `*addedK*` = the parent's added column K).
	var addedNames []string
	var anchorExprs, recursiveExprs []exprNode
	var filter *exprNode
	if s := c.Search; s != nil {
		k := len(addedNames)
		addedNames = append(addedNames, s.SetColumn)
		if s.BreadthFirst {
			// ROW(0, cols) / ROW(parent.f1 + 1, cols) — the depth, then the columns.
			first := []exprNode{scIntLit(0)}
			depth := scAddedCol(k)
			next := []exprNode{scBinary(opAdd, exprNode{Kind: exprFieldAccess, Base: &depth, Field: "f1"}, scIntLit(1))}
			for _, i := range searchCols {
				first = append(first, scUserCol(i))
				next = append(next, scUserCol(i))
			}
			anchorExprs = append(anchorExprs, exprNode{Kind: exprRow, RowItems: first})
			recursiveExprs = append(recursiveExprs, exprNode{Kind: exprRow, RowItems: next})
		} else {
			// ARRAY[ROW(cols)] / parent || ARRAY[ROW(cols)] — the path of rows seen.
			anchorExprs = append(anchorExprs, scArrayOf(scRowOf(searchCols)))
			recursiveExprs = append(recursiveExprs, scBinary(opConcat, scAddedCol(k), scArrayOf(scRowOf(searchCols))))
		}
	}
	if cy := c.Cycle; cy != nil && mark != nil {
		markK := len(addedNames)
		addedNames = append(addedNames, cy.MarkColumn)
		pathK := len(addedNames)
		addedNames = append(addedNames, cy.PathColumn)
		// The anchor's mark is the default, cast to the mark type when it widens.
		def := mark.def
		if !resolvedTypeEqual(mark.defTy, mark.ty) && mark.defTy.kind != rtNull {
			def = exprNode{Kind: exprCast, Cast: &castExpr{Inner: mark.def, TypeName: rtName(mark.ty)}}
		}
		anchorExprs = append(anchorExprs, def, scArrayOf(scRowOf(cycleCols)))
		// CASE WHEN ROW(cols) = ANY (parent path) THEN value ELSE default END.
		dflt := mark.def
		recursiveExprs = append(recursiveExprs, exprNode{Kind: exprCase, Case: &caseExpr{
			Whens: []caseWhen{{
				Cond: exprNode{Kind: exprQuantified, Quantified: &quantifiedExpr{
					Op: opEq, Lhs: scRowOf(cycleCols), Array: scAddedCol(pathK),
				}},
				Result: mark.value,
			}},
			Els: &dflt,
		}})
		recursiveExprs = append(recursiveExprs, scBinary(opConcat, scAddedCol(pathK), scArrayOf(scRowOf(cycleCols))))
		f := scBinary(opNe, scAddedCol(markK), mark.value)
		filter = &f
	}

	// Resolve over the anchor row; its types are the added columns' types.
	userNames := make([]string, len(userTypes))
	for i := range userTypes {
		userNames[i] = fmt.Sprintf("*%d*", i)
	}
	anchorRow, err := cteSyntheticTableCols("*anchor*", userNames, userTypes, nil, nil, nil)
	if err != nil {
		return nil, err
	}
	anchorScope := singleScope(db, anchorRow)
	plan := &searchCyclePlan{userCols: len(userTypes), names: addedNames}
	for _, e := range anchorExprs {
		r, t, err := resolve(anchorScope, e, nil, &aggCtx{}, ptypes)
		if err != nil {
			return nil, err
		}
		plan.anchor = append(plan.anchor, r)
		plan.types = append(plan.types, t)
	}
	// Resolve over the recursive row: the user columns, then the parent's added columns.
	rowNames := append([]string{}, userNames...)
	for k := range plan.types {
		rowNames = append(rowNames, fmt.Sprintf("*added%d*", k))
	}
	rowTypes := append(append([]resolvedType{}, userTypes...), plan.types...)
	recursiveRow, err := cteSyntheticTableCols("*recursive*", rowNames, rowTypes, nil, nil, nil)
	if err != nil {
		return nil, err
	}
	recursiveScope := singleScope(db, recursiveRow)
	for _, e := range recursiveExprs {
		r, _, err := resolve(recursiveScope, e, nil, &aggCtx{}, ptypes)
		if err != nil {
			return nil, err
		}
		plan.recursive = append(plan.recursive, r)
	}
	if filter != nil {
		r, _, err := resolve(recursiveScope, *filter, nil, &aggCtx{}, ptypes)
		if err != nil {
			return nil, err
		}
		plan.filter = r
	}
	return plan, nil
}

// searchCycleRows gives each row its SEARCH / CYCLE columns (recursive-cte.md §9): it evaluates exprs
// over the row and replaces everything past the CTE's own columns with the results. With a filter, a
// row is dropped unless the filter is TRUE. Each row is a cost-ceiling check point; the expressions
// charge their ordinary evaluation cost, returned as the second value.
func (db *engine) searchCycleRows(plan *searchCyclePlan, exprs []*rExpr, filter *rExpr, rows [][]Value, params []Value, outer []storedRow) ([][]Value, int64, error) {
	env := &evalEnv{exec: db, params: params, outer: outer, rng: newStmtRng()}
	meter := db.session.newMeter()
	acct := db.session.queryAccount()
	before := measureRows(acct, rows)
	out := make([][]Value, 0, len(rows))
	for _, row := range rows {
		if err := meter.Guard(); err != nil {
			return nil, 0, err
		}
		if filter != nil {
			v, err := filter.eval(row, env, meter)
			if err != nil {
				return nil, 0, err
			}
			if v.Kind != ValBool || !v.boolVal() {
				continue
			}
		}
		added := make([]Value, 0, len(exprs))
		for _, e := range exprs {
			v, err := e.eval(row, env, meter)
			if err != nil {
				return nil, 0, err
			}
			added = append(added, v)
		}
		next := make([]Value, 0, plan.userCols+len(added))
		next = append(next, row[:plan.userCols]...)
		out = append(out, append(next, added...))
	}
	if after := measureRows(acct, out); after >= before {
		if err := acct.reserve(after - before); err != nil {
			return nil, 0, err
		}
	} else {
		acct.release(before - after)
	}
	return out, meter.Accrued, nil
}

// starHiddenCount is how many trailing columns of rel `*` skips — a recursive CTE's SEARCH / CYCLE
// columns while its own recursive term is planned (recursive-cte.md §9).
func starHiddenCount(rel *scopeRel, ctes []*cteBinding) int {
	if rel.cte == nil {
		return 0
	}
	return ctes[*rel.cte].starHidden
}

// withCarriedColumns returns the recursive term with the self-reference's added columns appended as
// hidden trailing outputs (`self.seq`, `self.mark`, `self.path` — recursive-cte.md §9). The
// self-reference is a direct FROM / JOIN relation of the term's SELECT (analyzeRecursiveCte verified
// it); its label is its alias or the CTE's name.
func withCarriedColumns(term queryExpr, cteName string, added []string) queryExpr {
	sel := *term.Select
	label := ""
	refs := []*tableRef{sel.From}
	for i := range sel.Joins {
		refs = append(refs, &sel.Joins[i].Table)
	}
	for _, t := range refs {
		if t != nil && t.Subquery == nil && t.Args == nil && t.Values == nil && t.Name == cteName {
			label = t.Name
			if t.Alias != nil {
				label = *t.Alias
			}
			break
		}
	}
	sel.Tail = make([]exprNode, len(added))
	for k, name := range added {
		sel.Tail[k] = exprNode{Kind: exprQualifiedColumn, Qualifier: label, Column: name}
	}
	return queryExpr{Select: &sel}
}

// scClauseColumns returns the CTE column positions a SEARCH / CYCLE column list names
// (recursive-cte.md §9): each must be a column of the CTE (42601) and appear once (42701). A name the
// CTE has twice takes its first column, as in PostgreSQL.
func scClauseColumns(cols, names []string, what string, pos srcPos) ([]int, error) {
	out := make([]int, 0, len(cols))
	for i, col := range cols {
		at := slices.Index(names, col)
		if at < 0 {
			return nil, scClauseError(fmt.Sprintf("%s column \"%s\" not in WITH query column list", what, col), pos)
		}
		if slices.Contains(cols[:i], col) {
			return nil, newError(DuplicateColumn, fmt.Sprintf("%s column \"%s\" specified more than once", what, col)).withPosition(int32(pos))
		}
		out = append(out, at)
	}
	return out, nil
}

func scClauseError(msg string, pos srcPos) error {
	return newError(SyntaxError, msg).withPosition(int32(pos))
}
