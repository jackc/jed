package jed

import (
	"fmt"
	"strconv"
	"strings"
)

// EXPLAIN renders the planner's chosen plan as a deterministic, cross-core-identical result set
// (spec/design/explain.md). The output is an ordinary query outcome with three columns:
//
//	depth  i32   the plan node's nesting level (0-based), from a pre-order DFS of the plan tree
//	node   text  the operator label (a fixed vocabulary — the §8 cross-core spelling contract)
//	detail text  the node's attributes (access path, keys, counts); "-" when it has none
//
// Rows are emitted in pre-order, so the row order is deterministic by construction — the corpus
// asserts an EXPLAIN with `nosort` (a sanctioned use, like composite record_out). Plain EXPLAIN
// renders the plan WITHOUT executing the inner statement; EXPLAIN ANALYZE (a later slice) runs it.
//
// Every cell is non-empty and free of leading/trailing whitespace — the conformance harness renders
// the actual cell raw but TrimSpaces the expected line, so indentation is carried by `depth`, never
// by whitespace, and an empty detail uses the "-" sentinel (spec/design/explain.md §2).

// explainRender accumulates the rendered plan rows.
type explainRender struct {
	rows [][]Value
}

// emit appends one plan row. An empty detail becomes the "-" sentinel so no cell renders blank.
func (r *explainRender) emit(depth int, node, detail string) {
	if detail == "" {
		detail = "-"
	}
	r.rows = append(r.rows, []Value{IntValue(int64(depth)), TextValue(node), TextValue(detail)})
}

// executeExplain plans the inner statement and renders the plan (spec/design/explain.md). Plain
// EXPLAIN never executes the inner statement — planExplainInner produces the plan structs, which
// renderQueryPlan walks. The EXPLAIN statement's own cost is one row_produced per emitted plan row.
func (db *engine) executeExplain(ex *explain, params []Value) (outcome, error) {
	if ex.Analyze {
		return db.executeExplainAnalyze(ex.Inner, params)
	}
	if len(params) > 0 {
		// Plain EXPLAIN renders the plan structurally (a $N bound source prints as "$N", not its
		// bound value), so supplied parameters are neither needed nor bound.
		return outcome{}, newError(SyntaxError, "bind parameters are not allowed in EXPLAIN")
	}
	var r explainRender
	if err := db.renderExplain(&r, ex.Inner, 0); err != nil {
		return outcome{}, err
	}
	return db.explainOutcome(r.rows), nil
}

// executeExplainAnalyze renders the plan AND runs the inner statement, reporting the inner's ACTUAL
// accrued cost + row count on an "Analyze" root node (spec/design/explain.md §3). Because jed's cost
// meter is a deterministic cross-core contract, those figures are byte-identical across cores, so the
// output is corpus-assertable. The plan is rendered from the pre-execution catalog; then the inner
// statement executes for real (a DML inner mutates, and the outer autocommit commits it — EXPLAIN
// ANALYZE of a write IS a write, classified by stmtIsWrite). The EXPLAIN statement's OWN cost is one
// row_produced per emitted plan row (independent of the inner cost, which appears only in the root).
func (db *engine) executeExplainAnalyze(inner *statement, params []Value) (outcome, error) {
	// Render the plan tree first (plan-only, no execution — pre-mutation).
	var body explainRender
	if err := db.renderExplain(&body, inner, 0); err != nil {
		return outcome{}, err
	}
	// Execute the inner statement for real, capturing its actual accrued cost + row count. Privileges
	// and the lifetime budget were already admitted on the EXPLAIN (dispatchStmt recurses into the
	// inner), and the write gate / commit are handled by the outer autocommit.
	innerOut, err := db.dispatchStmtBody(*inner, params)
	if err != nil {
		return outcome{}, err
	}
	actualRows := int64(len(innerOut.Rows))
	if innerOut.Kind == outcomeStatement {
		actualRows = innerOut.RowsAffected // a DML statement without RETURNING
	}
	// Assemble: the Analyze root carries the actual figures; the plan tree sits one level deeper.
	var r explainRender
	r.emit(0, "Analyze", fmt.Sprintf("cost=%d rows=%d", innerOut.Cost, actualRows))
	for _, row := range body.rows {
		r.rows = append(r.rows, []Value{IntValue(row[0].Int + 1), row[1], row[2]})
	}
	return db.explainOutcome(r.rows), nil
}

// explainOutcome wraps rendered plan rows as a query outcome, charging the EXPLAIN's own cost — one
// row_produced per emitted plan row (a deterministic function of the plan-row count).
func (db *engine) explainOutcome(rows [][]Value) outcome {
	meter := db.session.newMeter()
	meter.Charge(costs.RowProduced * int64(len(rows)))
	return outcome{
		Kind:        outcomeQuery,
		ColumnNames: []string{"depth", "node", "detail"},
		ColumnTypes: []string{"i32", "text", "text"},
		Rows:        rows,
		Cost:        meter.Accrued,
	}
}

// renderExplain renders the plan for the inner statement (spec/design/explain.md). A DML statement is
// rendered by explainDml (plan-only — never executing, so an EXPLAIN of a DELETE deletes nothing); a
// read query is planned by planExplainInner and walked by renderQueryPlan.
func (db *engine) renderExplain(r *explainRender, inner *statement, depth int) error {
	switch {
	case inner.Insert != nil:
		return db.explainInsert(r, inner.Insert, depth)
	case inner.Update != nil:
		return db.explainUpdate(r, inner.Update, depth)
	case inner.Delete != nil:
		return db.explainDelete(r, inner.Delete, depth)
	default:
		qp, err := db.planExplainInner(inner)
		if err != nil {
			return err
		}
		return db.renderQueryPlan(r, qp, depth)
	}
}

// explainInsert renders an INSERT plan: the Insert root (with an ON CONFLICT note), then the row
// source — a planned SELECT subtree (INSERT … SELECT) or a Values leaf (INSERT … VALUES). It resolves
// the source but never writes.
func (db *engine) explainInsert(r *explainRender, ins *insert, depth int) error {
	if _, ok := db.lkpTable(ins.Table); !ok {
		return newError(UndefinedTable, "table does not exist: "+ins.Table)
	}
	r.emit(depth, "Insert "+ins.Table, insertDetail(ins))
	if ins.Select != nil {
		ptypes := &paramTypes{}
		plan, err := db.planQuery(queryExpr{Select: ins.Select}, nil, nil, ptypes)
		if err != nil {
			return err
		}
		return db.renderQueryPlan(r, plan, depth+1)
	}
	r.emit(depth+1, "Values", fmt.Sprintf("rows=%d", len(ins.Rows)))
	return nil
}

// insertDetail renders an INSERT's ON CONFLICT disposition (or "-" when there is none).
func insertDetail(ins *insert) string {
	if ins.OnConflict == nil {
		return "-"
	}
	if ins.OnConflict.DoUpdate {
		return "on conflict do update"
	}
	return "on conflict do nothing"
}

// explainUpdate renders an UPDATE plan: the Update root (with the assignment count), the residual
// Filter, then the target scan with its chosen access path. It resolves the WHERE and the scan bound
// via the same detectors the executor uses, but never writes.
func (db *engine) explainUpdate(r *explainRender, upd *update, depth int) error {
	table, ok := db.lkpTable(upd.Table)
	if !ok {
		return newError(UndefinedTable, "table does not exist: "+upd.Table)
	}
	filter, err := db.explainDmlFilter(table, upd.Filter)
	if err != nil {
		return err
	}
	r.emit(depth, "Update "+upd.Table, fmt.Sprintf("sets=%d", len(upd.Assignments)))
	db.renderDmlScan(r, table, upd.Table, filter, depth+1)
	return nil
}

// explainDelete renders a DELETE plan: the Delete root, the residual Filter, then the target scan
// with its chosen access path. It resolves the WHERE and the scan bound but never writes.
func (db *engine) explainDelete(r *explainRender, del *deleteStmt, depth int) error {
	table, ok := db.lkpTable(del.Table)
	if !ok {
		return newError(UndefinedTable, "table does not exist: "+del.Table)
	}
	filter, err := db.explainDmlFilter(table, del.Filter)
	if err != nil {
		return err
	}
	r.emit(depth, "Delete "+del.Table, "-")
	db.renderDmlScan(r, table, del.Table, filter, depth+1)
	return nil
}

// explainDmlFilter resolves an UPDATE/DELETE WHERE predicate against a single-table scope (the same
// prologue the executors use), or returns nil for a bare (no-WHERE) statement.
func (db *engine) explainDmlFilter(table *catTable, where *exprNode) (*rExpr, error) {
	if where == nil {
		return nil, nil
	}
	return resolveBooleanFilter(singleScope(db, table), where, &paramTypes{})
}

// renderDmlScan emits the residual Filter (when present) and the target Scan for an UPDATE/DELETE,
// choosing the access path with the SAME detectors the executor uses (PK bound, then GIN, then GiST —
// UPDATE/DELETE do not use secondary B-tree index bounds, indexes.md §5). The touched-set count is a
// DML cost detail left to EXPLAIN ANALYZE, so it is not shown here.
func (db *engine) renderDmlScan(r *explainRender, table *catTable, name string, filter *rExpr, depth int) {
	d := depth
	if filter != nil {
		r.emit(d, "Filter", fmt.Sprintf("conjuncts=%d", conjunctCount(filter)))
		d++
	}
	r.emit(d, "Scan "+name, db.scanDetail(name, db.dmlScanBound(table, filter), false, nil))
}

// dmlScanBound is EXPLAIN's compatibility wrapper over the typed mutation physical plan used by the
// executors. The unqualified explain surface has a nil database scope.
func (db *engine) dmlScanBound(table *catTable, filter *rExpr) *scanBound {
	return db.planMutationScan(nil, table, filter).bound
}

// planExplainInner resolves the inner statement into a queryPlan WITHOUT executing it. It handles the
// read-query forms — SELECT, a top-level set operation, and a read-only top-level WITH; DML is a later
// slice. A top-level WITH is planned as a nested WITH expression (there are no enclosing CTEs to
// inherit at the top level), which produces the same withPlan structure to render.
func (db *engine) planExplainInner(inner *statement) (queryPlan, error) {
	ptypes := &paramTypes{}
	switch {
	case inner.Select != nil:
		return db.planQuery(queryExpr{Select: inner.Select}, nil, nil, ptypes)
	case inner.SetOp != nil:
		return db.planQuery(queryExpr{SetOp: inner.SetOp}, nil, nil, ptypes)
	case inner.With != nil:
		body := inner.With.Body.AsQuery()
		if body == nil {
			// A data-modifying primary (writable CTE) — a DML EXPLAIN, handled in a later slice.
			return queryPlan{}, newError(FeatureNotSupported, "EXPLAIN of a data-modifying WITH is not yet supported")
		}
		wp, err := db.planWithExpr(&withExpr{Ctes: inner.With.Ctes, Recursive: inner.With.Recursive, Body: body}, nil, ptypes)
		if err != nil {
			return queryPlan{}, err
		}
		return queryPlan{with: wp}, nil
	default:
		return queryPlan{}, newError(FeatureNotSupported, "EXPLAIN of this statement is not yet supported")
	}
}

// renderQueryPlan walks a queryPlan arm at the given depth: a SELECT plan, a set operation, a VALUES
// relation, or a WITH plan.
func (db *engine) renderQueryPlan(r *explainRender, qp queryPlan, depth int) error {
	switch {
	case qp.sel != nil:
		return db.renderSelectPlan(r, qp.sel, depth)
	case qp.setop != nil:
		return db.renderSetOpPlan(r, qp.setop, depth)
	case qp.values != nil:
		db.renderValuesPlan(r, qp.values, depth)
		return nil
	case qp.with != nil:
		return db.renderWithPlan(r, qp.with, depth)
	default:
		return newError(FeatureNotSupported, "EXPLAIN of this query shape is not yet supported")
	}
}

// renderSelectPlan emits a selectPlan's nodes in operator order — outermost first, each the pre-order
// parent of the next, so the tree reads top-down as the executor's pipeline reads bottom-up: Limit,
// Sort, Distinct, Window, Aggregate, Filter (WHERE), then the FROM tree. A Sort is emitted only when
// the order is NOT elided; an elided ORDER BY (served by the scan / index / join order) is instead
// noted on the FROM tree's top node (spec/design/explain.md §5).
func (db *engine) renderSelectPlan(r *explainRender, sp *selectPlan, depth int) error {
	d := depth
	if sp.limit != nil || sp.offset != nil {
		r.emit(d, "Limit", limitDetail(sp.limit, sp.offset))
		d++
	}
	orderNote := ""
	if len(sp.order) > 0 {
		switch {
		case sp.phys.pkOrdered:
			orderNote = "pk ordered"
			if sp.phys.pkReverse {
				orderNote += " (reverse)"
			}
		case sp.phys.indexOrder != nil:
			orderNote = "index order: " + sp.phys.indexOrder.nameKey
		case sp.phys.joinPkOrdered:
			orderNote = "join pk ordered"
		default:
			r.emit(d, "Sort", fmt.Sprintf("keys=%d", len(sp.order)))
			d++
		}
	}
	if sp.distinct {
		r.emit(d, "Distinct", "-")
		d++
	}
	if sp.hasWindow {
		r.emit(d, "Window", fmt.Sprintf("funcs=%d", len(sp.windowSpecs)))
		d++
	}
	if sp.isAgg {
		r.emit(d, "Aggregate", aggDetail(sp))
		d++
	}
	if sp.filter != nil {
		r.emit(d, "Filter", fmt.Sprintf("conjuncts=%d", conjunctCount(sp.filter)))
		d++
	}
	return db.renderFrom(r, sp, d, orderNote)
}

// renderFrom emits the FROM tree: a left-deep chain of Nested Loop joins over the plan's relations,
// or a single relation leaf, or a Result node for a FROM-less query. orderNote, when non-empty,
// records an elided ORDER BY on the tree's top node.
func (db *engine) renderFrom(r *explainRender, sp *selectPlan, depth int, orderNote string) error {
	n := len(sp.rels)
	if n == 0 {
		r.emit(depth, "Result", withNote("-", orderNote))
		return nil
	}
	return db.renderJoinTree(r, sp, n, depth, orderNote)
}

// renderJoinTree emits the left-deep join over the first n relations: the outermost node is the last
// join (joins[n-2]), whose left subtree is the join over the first n-1 relations and whose right child
// is rels[n-1]. note tags the outermost node with an elided ORDER BY.
func (db *engine) renderJoinTree(r *explainRender, sp *selectPlan, n, depth int, note string) error {
	if n == 1 {
		return db.renderRelLeaf(r, sp, 0, depth, note)
	}
	j := sp.joins[n-2]
	r.emit(depth, "Nested Loop", withNote(joinDetail(j), note))
	if err := db.renderJoinTree(r, sp, n-1, depth+1, ""); err != nil {
		return err
	}
	return db.renderRelLeaf(r, sp, n-1, depth+1, "")
}

// renderRelLeaf emits one relation: a base-table Scan (with its access path), an SRF, a CTE Scan, or a
// Subquery (a derived table, whose inner plan recurses one level deeper). note tags a base Scan with
// an elided ORDER BY (only a single base-table relation can carry one).
func (db *engine) renderRelLeaf(r *explainRender, sp *selectPlan, i, depth int, note string) error {
	rel := sp.rels[i]
	switch {
	case rel.srf != nil && (rel.srf.kind == srfJedTables || rel.srf.kind == srfJedColumns ||
		rel.srf.kind == srfJedIndexes || rel.srf.kind == srfJedConstraints):
		// A catalog relation (introspection.md §5) is computed, not scanned — its own node name
		// (it is a relation, not a function) plus the database scope it reads.
		r.emit(depth, "Catalog Scan "+rel.tableName, withNote("db="+rel.srf.introspectScope, note))
		return nil
	case rel.srf != nil:
		r.emit(depth, "SRF "+rel.tableName, withNote("-", note))
		return nil
	case rel.cte != nil:
		r.emit(depth, "CTE Scan "+rel.tableName, withNote("-", note))
		return nil
	case rel.derived != nil:
		r.emit(depth, "Subquery "+rel.tableName, withNote("-", note))
		return db.renderQueryPlan(r, *rel.derived, depth+1)
	default:
		// An index-nested-loop bound (per-outer-row seek) takes precedence over the once-materialized
		// bound in the access-path label (cost.md §3 "JOIN").
		bound, inl := sp.phys.relBounds[i], false
		if sp.phys.relINLBounds[i] != nil {
			bound, inl = sp.phys.relINLBounds[i], true
		}
		r.emit(depth, "Scan "+rel.tableName, withNote(db.scanDetail(rel.tableName, bound, inl, sp.relMasks[i]), note))
		return nil
	}
}

// renderSetOpPlan emits a set operation: any trailing Limit / Sort on the combined result, the
// Union / Intersect / Except node, then the left and right operand plans as children.
func (db *engine) renderSetOpPlan(r *explainRender, sop *setOpPlan, depth int) error {
	d := depth
	if sop.limit != nil || sop.offset != nil {
		r.emit(d, "Limit", limitDetail(sop.limit, sop.offset))
		d++
	}
	if len(sop.order) > 0 {
		r.emit(d, "Sort", fmt.Sprintf("keys=%d", len(sop.order)))
		d++
	}
	r.emit(d, setOpNodeName(sop.op), setOpDetail(sop.all))
	if err := db.renderQueryPlan(r, sop.lhs, d+1); err != nil {
		return err
	}
	return db.renderQueryPlan(r, sop.rhs, d+1)
}

// renderValuesPlan emits a VALUES relation as a leaf node carrying its row count.
func (db *engine) renderValuesPlan(r *explainRender, vp *valuesPlan, depth int) {
	r.emit(depth, "Values", fmt.Sprintf("rows=%d", len(vp.rows)))
}

// renderWithPlan emits a WITH plan: the WITH node, each common-table expression as a CTE child (its
// body one level deeper), then the main body plan.
func (db *engine) renderWithPlan(r *explainRender, wp *withPlan, depth int) error {
	r.emit(depth, "WITH", fmt.Sprintf("ctes=%d", len(wp.bindings)))
	for i, b := range wp.bindings {
		mode := cteInline
		if i < len(wp.modes) {
			mode = wp.modes[i]
		}
		r.emit(depth+1, "CTE "+b.name, cteDetail(b, mode))
		if !b.isDml() {
			if err := db.renderQueryPlan(r, b.plan, depth+2); err != nil {
				return err
			}
		}
	}
	return db.renderQueryPlan(r, wp.body, depth+1)
}

// cteDetail renders a CTE binding's attributes: its materialization mode (inlined vs materialized —
// the planner's choice) and whether it is recursive.
func cteDetail(b *cteBinding, mode cteMode) string {
	parts := []string{cteModeText(mode)}
	if b.recursive != nil {
		parts = append(parts, "recursive")
	}
	return strings.Join(parts, "; ")
}

// cteModeText labels a CTE materialization mode.
func cteModeText(m cteMode) string {
	if m == cteMaterialize {
		return "materialized"
	}
	return "inlined"
}

// aggDetail renders an Aggregate node's attributes: the grouping-key count, aggregate count, the
// grouping-set count when there is more than one set, and the HAVING conjunct count.
func aggDetail(sp *selectPlan) string {
	parts := []string{fmt.Sprintf("groups=%d aggs=%d", len(sp.groupKeys), len(sp.aggSpecs))}
	if len(sp.groupSets) > 1 {
		parts = append(parts, fmt.Sprintf("sets=%d", len(sp.groupSets)))
	}
	if sp.having != nil {
		parts = append(parts, fmt.Sprintf("having:conjuncts=%d", conjunctCount(sp.having)))
	}
	return strings.Join(parts, "; ")
}

// joinDetail renders a Nested Loop node's attributes: the join kind and the ON predicate's conjunct
// count (a CROSS join has no ON).
func joinDetail(j planJoin) string {
	kind := joinKindText(j.kind)
	if j.on == nil {
		return kind
	}
	return fmt.Sprintf("%s; on:conjuncts=%d", kind, conjunctCount(j.on))
}

// joinKindText is the label for a join kind.
func joinKindText(k joinKind) string {
	switch k {
	case joinInner:
		return "inner"
	case joinCross:
		return "cross"
	case joinLeft:
		return "left"
	case joinRight:
		return "right"
	case joinFull:
		return "full"
	default:
		return "?"
	}
}

// setOpNodeName is the node label for a set-operation kind.
func setOpNodeName(op setOpKind) string {
	switch op {
	case setOpUnion:
		return "Union"
	case setOpIntersect:
		return "Intersect"
	case setOpExcept:
		return "Except"
	default:
		return "SetOp"
	}
}

// setOpDetail renders a set operation's ALL / DISTINCT disposition.
func setOpDetail(all bool) string {
	if all {
		return "all"
	}
	return "distinct"
}

// withNote appends an elided-ORDER-BY note to a node's detail (replacing a "-" sentinel).
func withNote(detail, note string) string {
	if note == "" {
		return detail
	}
	if detail == "" || detail == "-" {
		return "ordered: " + note
	}
	return detail + "; ordered: " + note
}

// scanDetail renders a Scan node's attributes: the access path (from the relation's chosen scan
// bound, nil = a full scan), then the touched-column count when the query references any column.
func (db *engine) scanDetail(tableName string, b *scanBound, inl bool, mask []bool) string {
	parts := []string{db.accessPath(tableName, b, inl)}
	if n := countTrue(mask); n > 0 {
		parts = append(parts, fmt.Sprintf("touched=%d", n))
	}
	return strings.Join(parts, "; ")
}

// accessPath renders the chosen access path for a relation (spec/design/explain.md §5): a full scan,
// a primary-key range bound, or a secondary-index / GIN / GiST bound (the last three by index name).
// inl marks an index-nested-loop bound (cost.md §3 "JOIN") — a per-outer-row seek whose source is a
// sibling column — with a leading label.
func (db *engine) accessPath(tableName string, b *scanBound, inl bool) string {
	prefix := ""
	if inl {
		prefix = "Index-nested-loop "
	}
	switch {
	case b == nil:
		return "Full scan"
	case b.pk != nil:
		return prefix + "PK bound: " + renderPKBound(b.pk)
	case b.index != nil:
		return prefix + "Index bound: using " + b.index.nameKey
	case b.gin != nil:
		return prefix + "GIN bound: using " + b.gin.nameKey
	case b.gist != nil:
		return prefix + "GiST bound: using " + b.gist.nameKey
	case b.pkSet != nil:
		return prefix + "PK interval set: " + db.firstPKColName(tableName) + "; intervals=" + strconv.Itoa(len(b.pkSet.specs))
	case b.indexSet != nil:
		return prefix + "Index interval set: using " + b.indexSet.nameKey + "; intervals=" + strconv.Itoa(len(b.indexSet.specs))
	default:
		return "Full scan"
	}
}

func renderPKBound(b *pkBoundPlan) string {
	parts := make([]string, 0, len(b.eqCols)+len(b.rangeTerms))
	for _, ec := range b.eqCols {
		for _, src := range ec.srcs {
			parts = append(parts, ec.name+" = "+renderBoundSrc(src))
		}
		if ec.ranges != nil {
			parts = append(parts, strings.Split(renderBoundTerms(ec.name, ec.ranges), " and ")...)
		}
	}
	if b.rangeTerms != nil {
		parts = append(parts, strings.Split(renderBoundTerms(b.rangeName, b.rangeTerms), " and ")...)
	}
	return strings.Join(parts, " and ")
}

// firstPKColName returns the name of a table's first primary-key column (in key order), or "pk" when
// the table is not found or has no primary key (a defensive fallback — the plan-only path already
// resolved the table, and a bounded scan implies a PK).
func (db *engine) firstPKColName(tableName string) string {
	if t, ok := db.lkpTable(tableName); ok && len(t.PK) > 0 && t.PK[0] < len(t.Columns) {
		return t.Columns[t.PK[0]].Name
	}
	return "pk"
}

// renderBoundTerms renders a primary-key bound's terms as `col <op> <src>` conjuncts joined by
// " and " — e.g. `id = $1`, `id >= 5 and id < 10`.
func renderBoundTerms(col string, terms []boundTerm) string {
	parts := make([]string, len(terms))
	for i, t := range terms {
		parts[i] = col + " " + boundOpText(t.op) + " " + renderBoundSrc(t.src)
	}
	return strings.Join(parts, " and ")
}

// boundOpText is the symbol for a bound comparison operator.
func boundOpText(op binaryOp) string {
	switch op {
	case opEq:
		return "="
	case opNe:
		return "<>"
	case opLt:
		return "<"
	case opGt:
		return ">"
	case opLe:
		return "<="
	case opGe:
		return ">="
	default:
		return "?"
	}
}

// renderBoundSrc renders a bound's const-source operand: a bind parameter as `$N` (1-based), a
// correlated outer-column reference as `outer`, or a literal via renderBoundLit.
func renderBoundSrc(e *rExpr) string {
	if e == nil {
		return "?"
	}
	switch e.kind {
	case reParam:
		return "$" + strconv.Itoa(e.index+1)
	case reOuterColumn:
		return "outer"
	case reColumn:
		// An index-nested-loop bound source — a column of an earlier join relation resolved per outer
		// row (cost.md §3 "JOIN"). Rendered generically (the global column index is not a user-facing
		// name, like the correlated `outer` case above).
		return "join"
	default:
		return renderBoundLit(e)
	}
}

// renderBoundLit renders a constant bound operand as a single-line token. Integer / boolean / decimal
// literals render deterministically; a float renders as the fixed token `<float>` (its layout is a
// determinism-ledger exception, kept out of the plan text — spec/design/explain.md §6); a text literal
// renders verbatim unless it contains a newline (which would split the cell), in which case `<text>`;
// every other constant type renders as `<value>` for now (a later slice widens this).
func renderBoundLit(e *rExpr) string {
	switch e.kind {
	case reConstInt:
		return strconv.FormatInt(e.cInt, 10)
	case reConstBool:
		if e.cBool {
			return "true"
		}
		return "false"
	case reConstDecimal:
		return e.cDec.Render()
	case reConstText:
		if strings.ContainsAny(e.cText, "\n\r") {
			return "<text>"
		}
		return "'" + e.cText + "'"
	case reConstFloat32, reConstFloat64:
		return "<float>"
	default:
		return "<value>"
	}
}

// conjunctCount counts the top-level AND conjuncts of a residual filter (a deterministic integer —
// the plan text carries the count, not the expression itself; a full expression printer is a later
// slice, spec/design/explain.md §5).
func conjunctCount(e *rExpr) int {
	if e == nil {
		return 0
	}
	if e.kind == reAnd {
		return conjunctCount(e.lhs) + conjunctCount(e.rhs)
	}
	return 1
}

// limitDetail renders a Limit node's `limit=N` / `offset=M` attributes (an absent side is omitted).
func limitDetail(limit, offset *int64) string {
	var parts []string
	if limit != nil {
		parts = append(parts, "limit="+strconv.FormatInt(*limit, 10))
	}
	if offset != nil {
		parts = append(parts, "offset="+strconv.FormatInt(*offset, 10))
	}
	if len(parts) == 0 {
		return "-"
	}
	return strings.Join(parts, " ")
}

// countTrue counts the set entries in a touched-set mask.
func countTrue(mask []bool) int {
	n := 0
	for _, b := range mask {
		if b {
			n++
		}
	}
	return n
}
