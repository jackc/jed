package jed

import (
	"fmt"
	"strconv"
	"strings"
)

// EXPLAIN renders the planner's chosen plan as a deterministic, cross-core-identical result set
// (spec/design/explain.md). The first three columns are structural; COSTS (the default), ANALYZE,
// and LANE append their deterministic columns:
//
//	depth  i32   the plan node's nesting level (0-based), from a pre-order DFS of the plan tree
//	node   text  the operator label (a fixed vocabulary — the §8 cross-core spelling contract)
//	detail text  the node's attributes (access path, keys, counts); "-" when it has none
//	est_rows i64 rows estimated to leave this node (affected rows for a DML root)
//	est_cost i64 cumulative scheduled cost estimated through this node
//
// Rows are emitted in pre-order, so the row order is deterministic by construction — the corpus
// asserts an EXPLAIN with `nosort` (a sanctioned use, like composite record_out). Plain EXPLAIN
// renders the plan WITHOUT executing the inner statement; EXPLAIN ANALYZE runs it.
//
// Every cell is non-empty and free of leading/trailing whitespace — the conformance harness renders
// the actual cell raw but TrimSpaces the expected line, so indentation is carried by `depth`, never
// by whitespace, and an empty detail uses the "-" sentinel (spec/design/explain.md §2).

// explainRender accumulates the rendered plan rows.
type explainRender struct {
	rows      [][]Value
	estimates []planEstimate
	actual    []int64
	verbose   bool
	next      int
	// frameDepths parallels rows and identifies the structural queryPlan frame that emitted each
	// node. Labels are not identities: an outer query and a derived table may both contain `Scan t`.
	frameDepths []int
	frameDepth  int
}

// emit appends one plan row. An empty detail becomes the "-" sentinel so no cell renders blank.
func (r *explainRender) emit(depth int, node, detail string) {
	if detail == "" {
		detail = "-"
	}
	estimate := planEstimate{}
	if r.next < len(r.estimates) {
		estimate = r.estimates[r.next]
	}
	r.next++
	r.rows = append(r.rows, []Value{
		IntValue(int64(depth)), TextValue(node), TextValue(detail),
		IntValue(estimate.rows), IntValue(estimate.cost()), IntValue(r.actualCost(r.next - 1)),
	})
	r.frameDepths = append(r.frameDepths, r.frameDepth)
}

func (r *explainRender) actualCost(i int) int64 {
	if i >= 0 && i < len(r.actual) {
		return r.actual[i]
	}
	return 0
}

// executeExplain plans the inner statement and renders the plan (spec/design/explain.md). Plain
// EXPLAIN never executes the inner statement — planExplainInner produces the plan structs, which
// renderQueryPlan walks. The EXPLAIN statement's own cost is one row_produced per emitted plan row.
func (db *engine) executeExplain(ex *explain, params []Value) (outcome, error) {
	if ex.Analyze {
		return db.executeExplainAnalyze(ex, params)
	}
	if len(params) > 0 {
		// Plain EXPLAIN renders the plan structurally (a $N bound source prints as "$N", not its
		// bound value), so supplied parameters are neither needed nor bound.
		return outcome{}, newError(SyntaxError, "bind parameters are not allowed in EXPLAIN")
	}
	estimates, err := db.estimateExplain(ex.Inner)
	if err != nil {
		return outcome{}, err
	}
	r := explainRender{estimates: estimates, verbose: ex.Verbose}
	if err := db.renderExplain(&r, ex.Inner, 0); err != nil {
		return outcome{}, err
	}
	return db.explainOutcome(r.rows, ex), nil
}

// executeExplainAnalyze renders the plan AND runs the inner statement, reporting the inner's ACTUAL
// accrued cost + row count on an "Analyze" root node (spec/design/explain.md §3). Because jed's cost
// meter is a deterministic cross-core contract, those figures are byte-identical across cores, so the
// output is corpus-assertable. The plan is rendered from the pre-execution catalog; then the inner
// statement executes for real (a DML inner mutates, and the outer autocommit commits it — EXPLAIN
// ANALYZE of a write IS a write, classified by stmtIsWrite). The EXPLAIN statement's OWN cost is one
// row_produced per emitted plan row (independent of the inner cost, which appears only in the root).
func (db *engine) executeExplainAnalyze(ex *explain, params []Value) (outcome, error) {
	inner := ex.Inner
	// Render the plan tree first (plan-only, no execution — pre-mutation).
	estimates, err := db.estimateExplain(inner)
	if err != nil {
		return outcome{}, err
	}
	body := explainRender{estimates: estimates, verbose: ex.Verbose}
	if err := db.renderExplain(&body, inner, 0); err != nil {
		return outcome{}, err
	}
	// Execute the inner statement for real, capturing its actual accrued cost + row count. Privileges
	// and the lifetime budget were already admitted on the EXPLAIN (dispatchStmt recurses into the
	// inner), and the write gate / commit are handled by the outer autocommit.
	profile := &actualCostProfile{byNode: make(map[actualCostKey][]int64)}
	previousProfile := db.explainActual
	db.explainActual = profile
	innerOut, err := db.dispatchStmtBody(*inner, params)
	db.explainActual = previousProfile
	if err != nil {
		return outcome{}, err
	}
	actualRows := int64(len(innerOut.Rows))
	if innerOut.Kind == outcomeStatement {
		actualRows = innerOut.RowsAffected // a DML statement without RETURNING
	}
	body.actual = explainActualCosts(estimates, innerOut.Cost)
	profile.apply(body.rows, body.frameDepths, body.actual)
	for i := range body.rows {
		body.rows[i][5] = IntValue(body.actualCost(i))
	}
	// Assemble: the Analyze root carries the actual figures; the plan tree sits one level deeper.
	r := explainRender{estimates: []planEstimate{}, actual: []int64{innerOut.Cost}}
	if len(estimates) > 0 {
		r.estimates = append(r.estimates, estimates[0])
	}
	r.emit(0, "Analyze", fmt.Sprintf("cost=%d rows=%d", innerOut.Cost, actualRows))
	for _, row := range body.rows {
		shifted := append([]Value{IntValue(row[0].Int + 1)}, row[1:]...)
		r.rows = append(r.rows, shifted)
	}
	return db.explainOutcome(r.rows, ex), nil
}

type actualCostKey struct {
	frame int
	node  string
}

type actualCostFrame struct {
	byNode    map[actualCostKey][]int64
	inclusive int64
}

type actualCostProfile struct {
	byNode       map[actualCostKey][]int64
	frames       []actualCostFrame
	folded       map[actualCostKey][]int64
	foldSuppress int
}

func (p *actualCostProfile) key(node string) actualCostKey {
	return actualCostKey{frame: len(p.frames), node: node}
}

func (p *actualCostProfile) record(node string, cost int64) {
	if p != nil {
		target := p.byNode
		if len(p.frames) > 0 {
			target = p.frames[len(p.frames)-1].byNode
		}
		key := p.key(node)
		target[key] = append(target[key], cost)
	}
}

// recordParent records an inclusive checkpoint for an operator after its children have run. The
// renderer walks parents before children, so parents of another occurrence with the same label are
// placed at the front of that label's deterministic queue.
func (p *actualCostProfile) recordParent(node string, cost int64) {
	if p != nil {
		target := p.byNode
		key := p.key(node)
		if len(p.frames) > 0 {
			frame := &p.frames[len(p.frames)-1]
			target = frame.byNode
			if pending := p.folded[key]; len(pending) > 0 {
				frame.inclusive += pending[0]
				p.folded[key] = pending[1:]
			}
			cost += frame.inclusive
		}
		target[key] = append([]int64{cost}, target[key]...)
	}
}

// recordFolded activates hidden subquery work at its containing visible operator. Execution records
// operators bottom-up, so once Filter is reached the same amount remains inclusive in every parent
// checkpoint above it. frame is the structural queryPlan depth that will execute after folding.
func (p *actualCostProfile) recordFolded(frame int, node string, cost int64) {
	if p == nil || p.foldSuppress > 0 || cost == 0 {
		return
	}
	if p.folded == nil {
		p.folded = make(map[actualCostKey][]int64)
	}
	key := actualCostKey{frame: frame, node: node}
	p.folded[key] = append(p.folded[key], cost)
}

func (p *actualCostProfile) beginFrame() {
	if p != nil {
		p.frames = append(p.frames, actualCostFrame{byNode: make(map[actualCostKey][]int64)})
	}
}

func (p *actualCostProfile) endFrame() {
	if p == nil || len(p.frames) == 0 {
		return
	}
	last := len(p.frames) - 1
	frame := p.frames[last]
	p.frames = p.frames[:last]
	target := p.byNode
	if len(p.frames) > 0 {
		target = p.frames[len(p.frames)-1].byNode
	}
	for key, costs := range frame.byNode {
		target[key] = append(target[key], costs...)
	}
}

func (p *actualCostProfile) discardFrame() {
	if p != nil && len(p.frames) > 0 {
		p.frames = p.frames[:len(p.frames)-1]
	}
}

func (p *actualCostProfile) apply(rows [][]Value, frameDepths []int, actual []int64) {
	used := make(map[actualCostKey]int)
	cteUsed := make(map[actualCostKey]int)
	suppressedDepth := -1
	for i, row := range rows {
		if len(row) < 2 || row[1].Kind != ValText {
			continue
		}
		depth := int(row[0].Int)
		if suppressedDepth >= 0 {
			if depth > suppressedDepth {
				continue
			}
			suppressedDepth = -1
		}
		node := row[1].str()
		frame := 0
		if i < len(frameDepths) {
			frame = frameDepths[i]
		}
		key := actualCostKey{frame: frame, node: node}
		values := p.byNode[key]
		at := used[key]
		if at < len(values) {
			if i != 0 {
				actual[i] = values[at]
			} // the rendered plan root already owns the exact whole-statement total
			used[key] = at + 1
		}
		if strings.HasPrefix(node, "CTE ") && !strings.HasPrefix(node, "CTE Scan ") {
			name := strings.TrimPrefix(node, "CTE ")
			markerKey := actualCostKey{frame: frame, node: "@cte-body " + name}
			markers := p.byNode[markerKey]
			at := cteUsed[markerKey]
			if at < len(markers) {
				if markers[at] == 0 {
					suppressedDepth = depth
				}
				cteUsed[markerKey] = at + 1
			}
		}
	}
}

// explainActualCosts seeds execution-only attribution. The root owns the exact statement total;
// every descendant starts at zero and is populated only by an execution checkpoint. In particular,
// an informational subtree under an unexecuted CTE definition must never inherit its estimate.
func explainActualCosts(estimates []planEstimate, total int64) []int64 {
	actual := make([]int64, len(estimates))
	if len(actual) > 0 {
		actual[0] = total
	}
	return actual
}

// explainOutcome wraps rendered plan rows as a query outcome, charging the EXPLAIN's own cost — one
// row_produced per emitted plan row (a deterministic function of the plan-row count).
func (db *engine) explainOutcome(rows [][]Value, ex *explain) outcome {
	meter := db.session.newMeter()
	meter.Charge(costs.RowProduced * int64(len(rows)))
	names := []string{"depth", "node", "detail"}
	types := []string{"i32", "text", "text"}
	lane := ""
	if ex.Lane {
		lane = db.explainLane(ex.Inner)
	}
	outRows := make([][]Value, len(rows))
	for i, row := range rows {
		outRows[i] = append([]Value{}, row[:3]...)
		if ex.Costs {
			outRows[i] = append(outRows[i], row[3], row[4])
		}
		if ex.Analyze {
			outRows[i] = append(outRows[i], row[5])
		}
		if ex.Lane {
			outRows[i] = append(outRows[i], TextValue(lane))
		}
	}
	if ex.Costs {
		names = append(names, "est_rows", "est_cost")
		types = append(types, "i64", "i64")
	}
	if ex.Analyze {
		names = append(names, "actual_cost")
		types = append(types, "i64")
	}
	if ex.Lane {
		names = append(names, "lane")
		types = append(types, "text")
	}
	return outcome{
		Kind:        outcomeQuery,
		ColumnNames: names,
		ColumnTypes: types,
		Rows:        outRows,
		Cost:        meter.Accrued,
	}
}

func (db *engine) explainLane(inner *statement) string {
	// Query checks stmtIsWrite before either lazy lane. A SELECT containing nextval/setval and a
	// top-level WITH containing DML therefore use the buffered write dispatcher regardless of shape.
	if stmtIsWrite(*inner) {
		return "buffered"
	}
	if inner.With != nil || inner.SetOp != nil {
		return "deferred"
	}
	if inner.Select != nil {
		plan, err := db.planExplainInner(inner)
		if err == nil && plan.sel != nil && pullStreamingScanEligible(plan.sel) {
			return "streaming"
		}
	}
	return "buffered"
}

// renderExplain renders the plan for the inner statement (spec/design/explain.md). A DML statement is
// rendered by explainDml (plan-only — never executing, so an EXPLAIN of a DELETE deletes nothing); a
// read query is planned by planExplainInner and walked by renderQueryPlan.
func (db *engine) renderExplain(r *explainRender, inner *statement, depth int) error {
	return db.renderExplainWithBindings(r, inner, depth, nil)
}

func (db *engine) renderExplainWithBindings(r *explainRender, inner *statement, depth int, bindings []*cteBinding) error {
	switch {
	case inner.Insert != nil:
		return db.explainInsert(r, inner.Insert, depth, bindings)
	case inner.Update != nil:
		return db.explainUpdate(r, inner.Update, depth, bindings)
	case inner.Delete != nil:
		return db.explainDelete(r, inner.Delete, depth, bindings)
	case inner.With != nil && withHasDml(inner.With):
		return db.renderExplainWithDml(r, inner.With, depth)
	default:
		qp, err := db.planExplainInner(inner)
		if err != nil {
			return err
		}
		// A top-level pure WITH is orchestrated directly by runWith rather than execQueryPlan, so its
		// wrapper is frame 0 and only its CTE/body query plans open structural execution frames.
		if inner.With != nil && qp.with != nil {
			return db.renderWithPlan(r, qp.with, depth)
		}
		return db.renderQueryPlan(r, qp, depth)
	}
}

type explainWithPlan struct {
	bindings []*cteBinding
	modes    []cteMode
	primary  *queryPlan
	body     cteBody
}

func (db *engine) planExplainWithDml(wq *withQuery) (*explainWithPlan, error) {
	ptypes := &paramTypes{}
	bindings, err := db.planCteBindings(wq.Ctes, wq.Recursive, nil, ptypes)
	if err != nil {
		return nil, err
	}
	var primary *queryPlan
	if q := wq.Body.AsQuery(); q != nil {
		plan, err := db.planQuery(*q, nil, bindings, ptypes)
		if err != nil {
			return nil, err
		}
		primary = &plan
	}
	for i := range wq.Ctes {
		if wq.Ctes[i].Body.IsDataModifying() {
			for _, binding := range bindings {
				binding.refs += countCteRefsDml(&wq.Ctes[i].Body, binding.name)
			}
		}
	}
	if wq.Body.IsDataModifying() {
		for _, binding := range bindings {
			binding.refs += countCteRefsDml(&wq.Body, binding.name)
		}
	}
	return &explainWithPlan{bindings: bindings, modes: cteModes(bindings), primary: primary, body: wq.Body}, nil
}

func (db *engine) renderExplainWithDml(r *explainRender, wq *withQuery, depth int) error {
	plan, err := db.planExplainWithDml(wq)
	if err != nil {
		return err
	}
	r.emit(depth, "WITH", fmt.Sprintf("ctes=%d", len(plan.bindings)))
	for i, binding := range plan.bindings {
		r.emit(depth+1, "CTE "+binding.name, cteDetail(binding, plan.modes[i]))
		if binding.isDml() {
			stmt := statement{Insert: binding.dm.insert, Update: binding.dm.update, Delete: binding.dm.delete}
			if err := db.renderExplainWithBindings(r, &stmt, depth+2, plan.bindings[:i]); err != nil {
				return err
			}
		} else if err := db.renderQueryPlan(r, binding.plan, depth+2); err != nil {
			return err
		}
	}
	if plan.primary != nil {
		return db.renderQueryPlan(r, *plan.primary, depth+1)
	}
	stmt := statement{Insert: plan.body.Insert, Update: plan.body.Update, Delete: plan.body.Delete}
	return db.renderExplainWithBindings(r, &stmt, depth+1, plan.bindings)
}

// explainInsert renders an INSERT plan: the Insert root (with an ON CONFLICT note), then the row
// source — a planned SELECT subtree (INSERT … SELECT) or a Values leaf (INSERT … VALUES). It resolves
// the source but never writes.
func (db *engine) explainInsert(r *explainRender, ins *insert, depth int, bindings []*cteBinding) error {
	if _, ok := db.lkpTable(ins.Table); !ok {
		return newError(UndefinedTable, "table does not exist: "+ins.Table)
	}
	r.emit(depth, "Insert "+ins.Table, insertDetail(ins))
	if ins.Select != nil {
		ptypes := &paramTypes{}
		plan, err := db.planQuery(queryExpr{Select: ins.Select}, nil, bindings, ptypes)
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
func (db *engine) explainUpdate(r *explainRender, upd *update, depth int, bindings []*cteBinding) error {
	table, ok := db.lkpTable(upd.Table)
	if !ok {
		return newError(UndefinedTable, "table does not exist: "+upd.Table)
	}
	filter, err := db.explainDmlFilter(table, upd.Filter, bindings)
	if err != nil {
		return err
	}
	r.emit(depth, "Update "+upd.Table, fmt.Sprintf("sets=%d", len(upd.Assignments)))
	mask, err := db.explainUpdateTouched(table, upd, filter, bindings)
	if err != nil {
		return err
	}
	db.renderDmlScan(r, table, upd.Table, filter, mask, depth+1)
	return nil
}

// explainDelete renders a DELETE plan: the Delete root, the residual Filter, then the target scan
// with its chosen access path. It resolves the WHERE and the scan bound but never writes.
func (db *engine) explainDelete(r *explainRender, del *deleteStmt, depth int, bindings []*cteBinding) error {
	table, ok := db.lkpTable(del.Table)
	if !ok {
		return newError(UndefinedTable, "table does not exist: "+del.Table)
	}
	filter, err := db.explainDmlFilter(table, del.Filter, bindings)
	if err != nil {
		return err
	}
	r.emit(depth, "Delete "+del.Table, "-")
	mask, err := db.explainDeleteTouched(table, del, filter, bindings)
	if err != nil {
		return err
	}
	db.renderDmlScan(r, table, del.Table, filter, mask, depth+1)
	return nil
}

// explainDmlFilter resolves an UPDATE/DELETE WHERE predicate against a single-table scope (the same
// prologue the executors use), or returns nil for a bare (no-WHERE) statement.
func (db *engine) explainDmlFilter(table *catTable, where *exprNode, bindings []*cteBinding) (*rExpr, error) {
	if where == nil {
		return nil, nil
	}
	s := singleScope(db, table)
	s.ctes = bindings
	return resolveBooleanFilter(s, where, &paramTypes{})
}

// renderDmlScan emits the residual Filter (when present) and the target Scan for an UPDATE/DELETE,
// choosing the access path with the SAME detectors the executor uses (PK bound, then GIN, then GiST —
// UPDATE/DELETE do not use secondary B-tree index bounds, indexes.md §5). The scan detail also
// reports the statement's resolved touched-set width.
func (db *engine) renderDmlScan(r *explainRender, table *catTable, name string, filter *rExpr, mask []bool, depth int) {
	d := depth
	if filter != nil {
		detail := fmt.Sprintf("conjuncts=%d", conjunctCount(filter))
		if r.verbose {
			detail = "filter=" + renderRExpr(filter)
		}
		r.emit(d, "Filter", detail)
		d++
	}
	r.emit(d, "Scan "+name, db.scanDetail(name, db.dmlScanBound(table, filter), false, mask))
}

func (db *engine) explainDeleteTouched(table *catTable, del *deleteStmt, filter *rExpr, bindings []*cteBinding) ([]bool, error) {
	mask := make([]bool, len(table.Columns))
	collectTouched(filter, 0, mask)
	if del.Returning == nil {
		return mask, nil
	}
	nodes, _, _, err := db.resolveReturning(table, *del.Returning, true, bindings, &paramTypes{})
	if err != nil {
		return nil, err
	}
	retMask := make([]bool, 2*len(mask))
	for _, node := range nodes {
		collectTouched(node, 0, retMask)
	}
	for i := range mask {
		mask[i] = mask[i] || retMask[i]
	}
	return mask, nil
}

func (db *engine) explainUpdateTouched(table *catTable, upd *update, filter *rExpr, bindings []*cteBinding) ([]bool, error) {
	mask := make([]bool, len(table.Columns))
	collectTouched(filter, 0, mask)
	s := singleScope(db, table)
	s.ctes = bindings
	ptypes := &paramTypes{}
	assigned := make([]bool, len(mask))
	for _, a := range upd.Assignments {
		idx := table.ColumnIndex(a.Column)
		if idx < 0 {
			return nil, newError(UndefinedColumn, "column does not exist: "+a.Column)
		}
		assigned[idx] = true
		if a.IsDefault {
			continue
		}
		col := table.Columns[idx]
		var node *rExpr
		var err error
		if scalar, ok := col.Type.AsScalar(); ok {
			node, _, err = resolve(s, a.Value, &scalar, &aggCtx{collecting: false}, ptypes)
		} else {
			node, err = resolveContainerAssign(s, col, a.Value, &aggCtx{collecting: false}, ptypes)
		}
		if err != nil {
			return nil, err
		}
		collectTouched(node, 0, mask)
	}
	if upd.Returning != nil {
		nodes, _, _, err := db.resolveReturning(table, *upd.Returning, false, bindings, ptypes)
		if err != nil {
			return nil, err
		}
		retMask := make([]bool, 2*len(mask))
		for _, node := range nodes {
			collectTouched(node, 0, retMask)
		}
		for i := range mask {
			mask[i] = mask[i] || retMask[len(mask)+i] || retMask[i] && !assigned[i]
		}
	}
	return mask, nil
}

// dmlScanBound is EXPLAIN's compatibility wrapper over the typed mutation physical plan used by the
// executors. The unqualified explain surface has a nil database scope.
func (db *engine) dmlScanBound(table *catTable, filter *rExpr) *scanBound {
	return db.planMutationScan(nil, table, filter).bound
}

// planExplainInner resolves the inner statement into a queryPlan WITHOUT executing it. It handles the
// read-query forms — SELECT, a top-level set operation, and a read-only top-level WITH. DML and a
// writable WITH are routed to their dedicated renderers before this helper. A read-only top-level
// WITH is planned as a nested WITH expression (there are no enclosing CTEs to
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
			return queryPlan{}, newError(FeatureNotSupported, "writable WITH requires the DML EXPLAIN renderer")
		}
		wp, err := db.planWithExpr(&withExpr{Ctes: inner.With.Ctes, Recursive: inner.With.Recursive, Body: body}, nil, nil, ptypes)
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
	r.frameDepth++
	defer func() { r.frameDepth-- }()
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
	start := len(r.rows)
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
			detail := fmt.Sprintf("keys=%d", len(sp.order))
			if sp.phys.topK != nil {
				detail += fmt.Sprintf(", top-k=%d", *sp.phys.topK)
			}
			r.emit(d, "Sort", detail)
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
		r.emit(d, "Aggregate", aggDetail(sp, r.verbose))
		d++
	}
	if sp.filter != nil {
		detail := fmt.Sprintf("conjuncts=%d", conjunctCount(sp.filter))
		if r.verbose {
			detail = "filter=" + renderRExpr(sp.filter)
		}
		r.emit(d, "Filter", detail)
		d++
	}
	if err := db.renderFrom(r, sp, d, orderNote); err != nil {
		return err
	}
	if r.verbose && len(r.rows) > start {
		parts := make([]string, len(sp.projections))
		for i, projection := range sp.projections {
			parts[i] = renderRExpr(projection)
		}
		detail := "output=[" + strings.Join(parts, ", ") + "]"
		old := r.rows[start][2].str()
		if old != "-" {
			detail = old + "; " + detail
		}
		r.rows[start][2] = TextValue(detail)
	}
	return nil
}

// selectActualRootNode mirrors renderSelectPlan's outermost-node choice. Execution records this
// checkpoint after emission, when projection and row_produced charges have joined the blocking
// pipeline, so ANALYZE can attribute the exact inclusive total to nested SELECT roots too.
func selectActualRootNode(sp *selectPlan) string {
	if sp.limit != nil || sp.offset != nil {
		return "Limit"
	}
	if len(sp.order) > 0 && !sp.phys.pkOrdered && sp.phys.indexOrder == nil && !sp.phys.joinPkOrdered {
		return "Sort"
	}
	if sp.distinct {
		return "Distinct"
	}
	if sp.hasWindow {
		return "Window"
	}
	if sp.isAgg {
		return "Aggregate"
	}
	if sp.filter != nil {
		return "Filter"
	}
	if len(sp.rels) > 1 {
		if len(sp.rels) >= 3 && len(sp.phys.joinSteps)+1 == len(sp.rels) {
			if sp.phys.joinSteps[len(sp.phys.joinSteps)-1].hashJoin != nil {
				return "Hash Join"
			}
		} else if len(sp.rels) == 2 && sp.phys.hashJoin != nil {
			return "Hash Join"
		}
		return "Nested Loop"
	}
	if len(sp.rels) == 0 {
		return "Result"
	}
	return selectActualRelNode(sp.rels[0])
}

func selectActualRelNode(rel planRel) string {
	if rel.srf != nil {
		if rel.srf.kind == srfJedTables || rel.srf.kind == srfJedColumns ||
			rel.srf.kind == srfJedIndexes || rel.srf.kind == srfJedConstraints ||
			rel.srf.kind == srfJedStatistics {
			return "Catalog Scan " + rel.tableName
		}
		return "SRF " + rel.tableName
	}
	if rel.cte != nil {
		return "CTE Scan " + rel.tableName
	}
	if rel.derived != nil {
		return "Subquery " + rel.tableName
	}
	return "Scan " + rel.tableName
}

// renderFrom emits the FROM tree: a left-deep chain of physical joins over the plan's relations,
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
	if len(sp.rels) >= 3 && len(sp.phys.relationOrder) == len(sp.rels) && len(sp.phys.joinSteps)+1 == len(sp.rels) {
		return db.renderNWayJoinTree(r, sp, n, depth, note)
	}
	if n == 1 {
		return db.renderRelLeaf(r, sp, 0, depth, note)
	}
	j := sp.joins[n-2]
	node := "Nested Loop"
	detail := joinDetail(j, r.verbose)
	if n == 2 && sp.phys.hashJoin != nil {
		node = "Hash Join"
		detail = fmt.Sprintf("%s; keys=%d", joinKindText(j.kind), len(sp.phys.hashJoin.keys))
		if j.on != nil {
			if r.verbose {
				detail += "; on=" + renderRExpr(j.on)
			} else {
				detail += fmt.Sprintf("; on:conjuncts=%d", conjunctCount(j.on))
			}
		}
	}
	r.emit(depth, node, withNote(detail, note))
	if n == 2 && len(sp.phys.relationOrder) == 2 {
		if err := db.renderRelLeaf(r, sp, sp.phys.relationOrder[0], depth+1, ""); err != nil {
			return err
		}
		return db.renderRelLeaf(r, sp, sp.phys.relationOrder[1], depth+1, "")
	}
	if err := db.renderJoinTree(r, sp, n-1, depth+1, ""); err != nil {
		return err
	}
	return db.renderRelLeaf(r, sp, n-1, depth+1, "")
}

func (db *engine) renderNWayJoinTree(r *explainRender, sp *selectPlan, n, depth int, note string) error {
	if n == 1 {
		return db.renderRelLeaf(r, sp, sp.phys.relationOrder[0], depth, note)
	}
	step := sp.phys.joinSteps[n-2]
	var ons []string
	conjuncts := 0
	for _, onIndex := range step.onIndices {
		if sp.joins[onIndex].on != nil {
			ons = append(ons, renderRExpr(sp.joins[onIndex].on))
			conjuncts += conjunctCount(sp.joins[onIndex].on)
		}
	}
	kind := joinKindText(physicalStepKind(sp, step))
	node := "Nested Loop"
	detail := kind
	if !r.verbose && len(ons) == 1 {
		detail = fmt.Sprintf("%s; on:conjuncts=%d", kind, conjuncts)
	} else if !r.verbose && len(ons) > 1 {
		detail = fmt.Sprintf("%s; on:predicates=%d,conjuncts=%d", kind, len(ons), conjuncts)
	} else if len(ons) == 1 {
		detail = kind + "; on=" + ons[0]
	} else if len(ons) > 1 {
		detail = kind + "; on=[" + strings.Join(ons, ", ") + "]"
	}
	if step.hashJoin != nil {
		node = "Hash Join"
		detail = fmt.Sprintf("%s; keys=%d", kind, len(step.hashJoin.keys))
		if !r.verbose && len(ons) == 1 {
			detail += fmt.Sprintf("; on:conjuncts=%d", conjuncts)
		} else if !r.verbose && len(ons) > 1 {
			detail += fmt.Sprintf("; on:predicates=%d,conjuncts=%d", len(ons), conjuncts)
		} else if len(ons) == 1 {
			detail += "; on=" + ons[0]
		} else if len(ons) > 1 {
			detail += "; on=[" + strings.Join(ons, ", ") + "]"
		}
	}
	r.emit(depth, node, withNote(detail, note))
	if err := db.renderNWayJoinTree(r, sp, n-1, depth+1, ""); err != nil {
		return err
	}
	return db.renderRelLeaf(r, sp, sp.phys.relationOrder[n-1], depth+1, "")
}

// renderRelLeaf emits one relation: a base-table Scan (with its access path), an SRF, a CTE Scan, or a
// Subquery (a derived table, whose inner plan recurses one level deeper). note tags a base Scan with
// an elided ORDER BY (only a single base-table relation can carry one).
func (db *engine) renderRelLeaf(r *explainRender, sp *selectPlan, i, depth int, note string) error {
	rel := sp.rels[i]
	switch {
	case rel.srf != nil && (rel.srf.kind == srfJedTables || rel.srf.kind == srfJedColumns ||
		rel.srf.kind == srfJedIndexes || rel.srf.kind == srfJedConstraints ||
		rel.srf.kind == srfJedStatistics):
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
func aggDetail(sp *selectPlan, verbose bool) string {
	parts := []string{fmt.Sprintf("groups=%d aggs=%d", len(sp.groupKeys), len(sp.aggSpecs))}
	if len(sp.groupSets) > 1 {
		parts = append(parts, fmt.Sprintf("sets=%d", len(sp.groupSets)))
	}
	if sp.having != nil {
		if verbose {
			parts = append(parts, "having="+renderRExpr(sp.having))
		} else {
			parts = append(parts, fmt.Sprintf("having:conjuncts=%d", conjunctCount(sp.having)))
		}
	}
	return strings.Join(parts, "; ")
}

// joinDetail renders a Nested Loop node's attributes: the join kind and the ON predicate's conjunct
// count (a CROSS join has no ON).
func joinDetail(j planJoin, verbose bool) string {
	kind := joinKindText(j.kind)
	if j.on == nil {
		return kind
	}
	if verbose {
		return kind + "; on=" + renderRExpr(j.on)
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

// renderBoundLit renders a constant bound operand as a single-line token. Floats use the native
// shortest-round-trip spelling under explain-float-literal-layout; other values use their canonical
// renderer, quoting textual forms so the token stays structurally unambiguous.
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
		return quoteExplainText(e.cText)
	case reConstFloat32, reConstFloat64:
		return strconv.FormatFloat(e.cFloat, 'g', -1, map[bool]int{true: 32, false: 64}[e.kind == reConstFloat32])
	default:
		return "<value>"
	}
}

func quoteExplainText(s string) string {
	s = strings.ReplaceAll(s, "\\", "\\\\")
	s = strings.ReplaceAll(s, "'", "''")
	s = strings.ReplaceAll(s, "\n", "\\n")
	s = strings.ReplaceAll(s, "\r", "\\r")
	s = strings.ReplaceAll(s, "\t", "\\t")
	return "'" + s + "'"
}

func explainBinaryOp(op binaryOp) string {
	switch op {
	case opAdd:
		return "+"
	case opSub:
		return "-"
	case opMul:
		return "*"
	case opDiv:
		return "/"
	case opMod:
		return "%"
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
	case opAnd:
		return "and"
	case opOr:
		return "or"
	}
	return "?"
}

func explainExprCall(name string, args []*rExpr) string {
	parts := make([]string, len(args))
	for i, arg := range args {
		parts[i] = renderRExpr(arg)
	}
	return name + "(" + strings.Join(parts, ", ") + ")"
}

// renderRExpr is EXPLAIN's canonical resolved-expression printer. Every compound is fully
// parenthesized and resolved columns use structural zero-based slots, never source aliases.
func renderRExpr(e *rExpr) string {
	if e == nil {
		return "NULL"
	}
	binary := func(op string) string { return "(" + renderRExpr(e.lhs) + " " + op + " " + renderRExpr(e.rhs) + ")" }
	switch e.kind {
	case reColumn:
		return fmt.Sprintf("#%d", e.index)
	case reOuterColumn:
		return fmt.Sprintf("outer(%d,%d)", e.level, e.index)
	case reParam:
		return fmt.Sprintf("$%d", e.index+1)
	case reConstInt:
		return strconv.FormatInt(e.cInt, 10)
	case reConstBool:
		if e.cBool {
			return "true"
		}
		return "false"
	case reConstText:
		return quoteExplainText(e.cText)
	case reConstDecimal:
		return e.cDec.Render()
	case reConstBytea:
		return quoteExplainText(ByteaValue(e.cBytea).Render())
	case reConstUuid:
		return quoteExplainText(UuidValue(e.cBytea).Render())
	case reConstTimestamp:
		return quoteExplainText(TimestampValue(e.cInt).Render())
	case reConstTimestamptz:
		return quoteExplainText(TimestamptzValue(e.cInt).Render())
	case reConstDate:
		return quoteExplainText(DateValue(int32(e.cInt)).Render())
	case reConstInterval:
		return quoteExplainText(IntervalValue(e.cIv).Render())
	case reConstFloat32:
		return Float32Value(float32(e.cFloat)).Render()
	case reConstFloat64:
		return Float64Value(e.cFloat).Render()
	case reConstJson, reConstJsonPath:
		return quoteExplainText(e.cText)
	case reConstJsonb:
		if e.cJsonb != nil {
			return quoteExplainText(JsonbValue(*e.cJsonb).Render())
		}
		return "NULL"
	case reConstNull:
		return "NULL"
	case reConstArray:
		if e.cArray != nil {
			return quoteExplainText(Value{Kind: ValArray, ref: e.cArray}.Render())
		}
		return "NULL"
	case reConstRange:
		if e.cRange != nil {
			return quoteExplainText(RangeValue(e.cRange).Render())
		}
		return "NULL"
	case reRow:
		return explainExprCall("row", e.sargs)
	case reArray:
		return "array[" + strings.TrimSuffix(strings.TrimPrefix(explainExprCall("", e.sargs), "("), ")") + "]"
	case reField:
		return fmt.Sprintf("%s.field%d", renderRExpr(e.operand), e.index)
	case reSubscript:
		var b strings.Builder
		b.WriteString(renderRExpr(e.operand))
		for _, sub := range e.subs {
			b.WriteByte('[')
			if sub.isSlice {
				if sub.lower != nil {
					b.WriteString(renderRExpr(sub.lower))
				}
				b.WriteByte(':')
				if sub.upper != nil {
					b.WriteString(renderRExpr(sub.upper))
				}
			} else {
				b.WriteString(renderRExpr(sub.index))
			}
			b.WriteByte(']')
		}
		return b.String()
	case reCast:
		return "cast(" + renderRExpr(e.operand) + " as " + e.result.CanonicalName() + ")"
	case reArrayCast:
		return "array_cast(" + renderRExpr(e.operand) + ")"
	case reNeg:
		return "(-" + renderRExpr(e.operand) + ")"
	case reNot:
		return "(not " + renderRExpr(e.operand) + ")"
	case reArith, reCompare:
		return binary(explainBinaryOp(e.op))
	case reAnd:
		return binary("and")
	case reOr:
		return binary("or")
	case reIsNull:
		if e.negated {
			return "(" + renderRExpr(e.operand) + " is not null)"
		}
		return "(" + renderRExpr(e.operand) + " is null)"
	case reIsJson:
		return fmt.Sprintf("(%s is %sjson%s%s)", renderRExpr(e.operand), map[bool]string{true: "not ", false: ""}[e.negated], explainJSONPredicate(e.jpKind), map[bool]string{true: " with unique keys", false: ""}[e.jpUnique])
	case reJsonCtor:
		return fmt.Sprintf("json(%s%s)", renderRExpr(e.operand), map[bool]string{true: " with unique keys", false: ""}[e.jpUnique])
	case reDistinct:
		if e.negated {
			return binary("is not distinct from")
		}
		return binary("is distinct from")
	case reLike:
		op := "like"
		if e.insensitive {
			op = "ilike"
		}
		if e.negated {
			op = "not " + op
		}
		return binary(op)
	case reRegex:
		op := "~"
		if e.negated {
			op = "!~"
		}
		if e.insensitive {
			op += "*"
		}
		return binary(op)
	case reCasing:
		if e.casingUpper {
			return explainExprCall("upper", []*rExpr{e.operand})
		}
		return explainExprCall("lower", []*rExpr{e.operand})
	case reAtTimeZone:
		return "(" + renderRExpr(e.rhs) + " at time zone " + renderRExpr(e.lhs) + ")"
	case reDateTrunc:
		return explainExprCall("date_trunc", e.sargs)
	case reExtract:
		return "extract(" + e.cText + " from " + renderRExpr(e.operand) + ")"
	case reDateConvert:
		return "cast(" + renderRExpr(e.operand) + " as " + e.result.CanonicalName() + ")"
	case reDateClock:
		return fmt.Sprintf("date_clock(%d)", e.cInt)
	case reCase:
		var b strings.Builder
		b.WriteString("(case")
		for _, arm := range e.caseArms {
			b.WriteString(" when ")
			b.WriteString(renderRExpr(arm.cond))
			b.WriteString(" then ")
			b.WriteString(renderRExpr(arm.result))
		}
		b.WriteString(" else ")
		b.WriteString(renderRExpr(e.caseEls))
		b.WriteString(" end)")
		return b.String()
	case reCoalesce:
		return explainExprCall("coalesce", e.sargs)
	case reGreatestLeast:
		if e.greatest {
			return explainExprCall("greatest", e.sargs)
		}
		return explainExprCall("least", e.sargs)
	case reScalarFunc:
		return explainExprCall(explainScalarFunc(e.sfunc), e.sargs)
	case reHostFunc:
		// The host function name is carried on the node (cText) so EXPLAIN renders it without the
		// registry (extensibility.md §5.1).
		return explainExprCall(e.cText, e.sargs)
	case reArrayFunc:
		return explainExprCall(explainArrayFunc(e.afunc), e.sargs)
	case reRangeFunc:
		return explainExprCall(explainRangeFunc(e.rfunc), e.sargs)
	case reRegexFunc:
		return explainExprCall(explainRegexFunc(e.rxFunc), e.sargs)
	case reRangeCtor:
		name, _ := rangeNameForElement(e.relem)
		return explainExprCall(name, e.sargs)
	case reRangeOp:
		return explainExprCall("range_"+explainRangeOp(e.rop), e.sargs)
	case reRangeSetOp:
		return explainExprCall("range_"+explainRangeSetOp(e.rsop), e.sargs)
	case reVariadic:
		return explainExprCall(explainVariadicFunc(e.vfunc), e.sargs)
	case reJsonBuild:
		family := "jsonb"
		if e.jbJson {
			family = "json"
		}
		return explainExprCall(family+"_build_"+explainJSONBuildKind(e.jbKind), e.sargs)
	case reJsonSetInsert:
		name := "jsonb_set"
		if e.psMode == psInsert {
			name = "jsonb_insert"
		}
		return explainExprCall(name, e.sargs)
	case reJsonObject:
		name := "jsonb_object"
		if e.jbJson {
			name = "json_object"
		}
		return explainExprCall(name, e.sargs)
	case reJsonPathFn:
		return explainExprCall(explainJSONPathFunc(e.jpFnKind), e.sargs)
	case reJsonSqlFn:
		return renderExplainJSONSQL(e)
	case reJsonGet:
		return binary(explainJSONGetOp(e.jgop))
	case reJsonContains:
		return binary("@>")
	case reJsonHasKey:
		return binary(explainJSONHasKey(e.hasKey))
	case reJsonConcat:
		return binary("||")
	case reJsonDelete:
		return binary(explainJSONDelete(e.delKind))
	case reSubquery:
		return explainSubquery(e)
	case reInValues:
		vals := make([]string, len(e.list))
		for i := range e.list {
			vals[i] = renderExplainValue(e.list[i])
		}
		return fmt.Sprintf("%s %sin (%s)", renderRExpr(e.lhs), map[bool]string{true: "not ", false: ""}[e.negated], strings.Join(vals, ", "))
	case reQuantified:
		return fmt.Sprintf("%s %s %s(%s)", renderRExpr(e.lhs), explainBinaryOp(e.op), map[bool]string{true: "all", false: "any"}[e.quantAll], renderRExpr(e.rhs))
	}
	panic("unhandled resolved expression in EXPLAIN")
}

func explainScalarFunc(f scalarFunc) string {
	switch f {
	case sfAbs, sfFloatAbs:
		return "abs"
	case sfRound, sfFloatRound:
		return "round"
	case sfCeil:
		return "ceil"
	case sfFloor:
		return "floor"
	case sfTrunc:
		return "trunc"
	case sfSqrt:
		return "sqrt"
	case sfExp:
		return "exp"
	case sfLn:
		return "ln"
	case sfLog10:
		return "log10"
	case sfPow:
		return "pow"
	case sfLog:
		return "log"
	case sfSin:
		return "sin"
	case sfCos:
		return "cos"
	case sfTan:
		return "tan"
	case sfCbrt:
		return "cbrt"
	case sfPi:
		return "pi"
	case sfRadians:
		return "radians"
	case sfDegrees:
		return "degrees"
	case sfAsin:
		return "asin"
	case sfAcos:
		return "acos"
	case sfAtan:
		return "atan"
	case sfAtan2:
		return "atan2"
	case sfCot:
		return "cot"
	case sfSinh:
		return "sinh"
	case sfCosh:
		return "cosh"
	case sfTanh:
		return "tanh"
	case sfAsinh:
		return "asinh"
	case sfAcosh:
		return "acosh"
	case sfAtanh:
		return "atanh"
	case sfSign:
		return "sign"
	case sfDiv:
		return "div"
	case sfGcd:
		return "gcd"
	case sfLcm:
		return "lcm"
	case sfFactorial:
		return "factorial"
	case sfWidthBucket:
		return "width_bucket"
	case sfScale:
		return "scale"
	case sfMinScale:
		return "min_scale"
	case sfTrimScale:
		return "trim_scale"
	case sfMakeInterval:
		return "make_interval"
	case sfMakeTimestamp:
		return "make_timestamp"
	case sfMakeTimestamptz:
		return "make_timestamptz"
	case sfMakeDate:
		return "make_date"
	case sfCurrentDate:
		return "current_date"
	case sfDatePart:
		return "date_part"
	case sfUuidExtractVersion:
		return "uuid_extract_version"
	case sfUuidExtractTimestamp:
		return "uuid_extract_timestamp"
	case sfUuidv4:
		return "uuidv4"
	case sfUuidv7:
		return "uuidv7"
	case sfNow:
		return "now"
	case sfClockTimestamp:
		return "clock_timestamp"
	case sfNextval:
		return "nextval"
	case sfCurrval:
		return "currval"
	case sfSetval:
		return "setval"
	case sfLastval:
		return "lastval"
	case sfCurrentSetting:
		return "current_setting"
	case sfJsonbTypeof:
		return "jsonb_typeof"
	case sfJsonTypeof:
		return "json_typeof"
	case sfJsonbArrayLength:
		return "jsonb_array_length"
	case sfJsonArrayLength:
		return "json_array_length"
	case sfJsonbStripNulls:
		return "jsonb_strip_nulls"
	case sfJsonStripNulls:
		return "json_strip_nulls"
	case sfJsonbPretty:
		return "jsonb_pretty"
	case sfToJsonb:
		return "to_jsonb"
	case sfToJson:
		return "to_json"
	case sfJsonScalar:
		return "json_scalar"
	case sfJsonSerialize:
		return "json_serialize"
	case sfLength:
		return "length"
	case sfOctetLength:
		return "octet_length"
	case sfBitLength:
		return "bit_length"
	case sfSubstr:
		return "substr"
	case sfLeft:
		return "left"
	case sfRight:
		return "right"
	case sfLpad:
		return "lpad"
	case sfRpad:
		return "rpad"
	case sfBtrim:
		return "btrim"
	case sfLtrim:
		return "ltrim"
	case sfRtrim:
		return "rtrim"
	case sfReplace:
		return "replace"
	case sfTranslate:
		return "translate"
	case sfRepeat:
		return "repeat"
	case sfReverse:
		return "reverse"
	case sfStrpos:
		return "strpos"
	case sfSplitPart:
		return "split_part"
	case sfStartsWith:
		return "starts_with"
	case sfAscii:
		return "ascii"
	case sfChr:
		return "chr"
	case sfInitcap:
		return "initcap"
	case sfToHex:
		return "to_hex"
	case sfEncode:
		return "encode"
	case sfDecode:
		return "decode"
	case sfQuoteLiteral:
		return "quote_literal"
	case sfQuoteIdent:
		return "quote_ident"
	case sfQuoteNullable:
		return "quote_nullable"
	}
	panic("unhandled scalar function in EXPLAIN")
}

func explainArrayFunc(f arrayFunc) string {
	return [...]string{"array_ndims", "array_length", "array_lower", "array_upper", "cardinality", "array_dims", "array_append", "array_prepend", "array_cat", "array_remove", "array_replace", "array_position", "array_positions", "array_to_json", "contains", "contained_by", "overlaps"}[f]
}

func explainRangeFunc(f rangeFunc) string {
	return [...]string{"lower", "upper", "isempty", "lower_inc", "upper_inc", "lower_inf", "upper_inf"}[f]
}

func explainRegexFunc(f regexFunc) string {
	return [...]string{"regexp_replace", "regexp_match", "regexp_like", "regexp_count", "regexp_substr", "regexp_instr"}[f]
}

func explainRangeOp(op rangeOp) string {
	return [...]string{"contains", "contains_elem", "contained_by", "elem_contained_by", "overlaps", "before", "after", "overleft", "overright", "adjacent"}[op]
}

func explainRangeSetOp(op rangeSetOp) string {
	return [...]string{"union", "intersect", "difference", "merge"}[op]
}

func explainVariadicFunc(f variadicFunc) string {
	return [...]string{"num_nulls", "num_nonnulls"}[f]
}

func explainJSONBuildKind(k jsonBuildKind) string {
	return [...]string{"array", "object"}[k]
}

func explainJSONPathFunc(k jsonPathFnKind) string {
	return [...]string{"jsonb_path_exists", "jsonb_path_query_first", "jsonb_path_query_array", "jsonb_path_match", "jsonb_path_match_silent"}[k]
}

func explainJSONSQLFunc(k jsonSqlKind) string {
	return [...]string{"json_exists", "json_value", "json_query"}[k]
}

func renderExplainJSONSQL(e *rExpr) string {
	var b strings.Builder
	b.WriteString(explainJSONSQLFunc(e.jsKind))
	b.WriteByte('(')
	b.WriteString(renderRExpr(e.sargs[0]))
	b.WriteString(", ")
	b.WriteString(renderRExpr(e.sargs[1]))
	switch e.jsKind {
	case jsExists:
		b.WriteByte(' ')
		b.WriteString(explainJSONBehavior(e.jsOnError))
		b.WriteString(" on error")
	case jsValue:
		b.WriteString(" returning ")
		b.WriteString(e.result.CanonicalName())
		b.WriteByte(' ')
		b.WriteString(explainJSONBehavior(e.jsOnEmpty))
		b.WriteString(" on empty ")
		b.WriteString(explainJSONBehavior(e.jsOnError))
		b.WriteString(" on error")
	case jsQuery:
		b.WriteString(" returning ")
		b.WriteString(e.result.CanonicalName())
		b.WriteByte(' ')
		b.WriteString(explainJSONWrapper(e.jsWrapper))
		if e.jsKeepQuotes {
			b.WriteString(" keep quotes on scalar string")
		} else {
			b.WriteString(" omit quotes on scalar string")
		}
		b.WriteByte(' ')
		b.WriteString(explainJSONBehavior(e.jsOnEmpty))
		b.WriteString(" on empty ")
		b.WriteString(explainJSONBehavior(e.jsOnError))
		b.WriteString(" on error")
	}
	b.WriteByte(')')
	return b.String()
}

func explainJSONWrapper(w jsonWrapper) string {
	return [...]string{
		"without array wrapper",
		"with unconditional array wrapper",
		"with conditional array wrapper",
	}[w]
}

func explainJSONBehavior(b jsonOnBehavior) string {
	return [...]string{
		"null", "error", "true", "false", "unknown", "empty array", "empty object",
	}[b]
}

func explainJSONGetOp(op jsonGetOp) string {
	return [...]string{"->", "->>", "#>", "#>>"}[op]
}

func explainJSONHasKey(k hasKeyKind) string {
	return [...]string{"?", "?|", "?&"}[k]
}

func explainJSONDelete(k deleteKind) string {
	if k == dkPath {
		return "#-"
	}
	return "-"
}

func explainJSONPredicate(k jsonPredicateKind) string {
	return [...]string{"", " scalar", " array", " object"}[k]
}

func explainSubquery(e *rExpr) string {
	switch e.subKind {
	case sqScalar:
		return "scalar(<subquery>)"
	case sqExists:
		return map[bool]string{true: "not exists(<subquery>)", false: "exists(<subquery>)"}[e.negated]
	case sqIn:
		return fmt.Sprintf("%s %sin (<subquery>)", renderRExpr(e.lhs), map[bool]string{true: "not ", false: ""}[e.negated])
	case sqQuantified:
		return fmt.Sprintf("%s %s %s(<subquery>)", renderRExpr(e.lhs), explainBinaryOp(e.op), map[bool]string{true: "all", false: "any"}[e.quantAll])
	default:
		panic("unhandled subquery kind in EXPLAIN")
	}
}

func renderExplainValue(v Value) string {
	switch v.Kind {
	case ValNull:
		return "NULL"
	case ValInt, ValBool, ValDecimal, ValFloat32, ValFloat64:
		return v.Render()
	default:
		return quoteExplainText(v.Render())
	}
}

// conjunctCount retains the compact non-VERBOSE spelling for a residual filter. VERBOSE uses the
// complete resolved-expression printer above (spec/design/explain.md §5).
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
