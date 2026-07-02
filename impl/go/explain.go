package jed

import (
	"fmt"
	"strconv"
	"strings"
)

// EXPLAIN renders the planner's chosen plan as a deterministic, cross-core-identical result set
// (spec/design/explain.md). The output is an ordinary query Outcome with three columns:
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
func (db *engine) executeExplain(ex *explain, params []Value) (Outcome, error) {
	if ex.Analyze {
		return Outcome{}, newError(FeatureNotSupported, "EXPLAIN ANALYZE is not yet supported")
	}
	if len(params) > 0 {
		// Plain EXPLAIN renders the plan structurally (a $N bound source prints as "$N", not its
		// bound value), so supplied parameters are neither needed nor bound.
		return Outcome{}, newError(SyntaxError, "bind parameters are not allowed in EXPLAIN")
	}
	qp, err := db.planExplainInner(ex.Inner)
	if err != nil {
		return Outcome{}, err
	}
	var r explainRender
	if err := db.renderQueryPlan(&r, qp, 0); err != nil {
		return Outcome{}, err
	}
	meter := db.session.newMeter()
	meter.Charge(costs.RowProduced * int64(len(r.rows)))
	return Outcome{
		Kind:        OutcomeQuery,
		ColumnNames: []string{"depth", "node", "detail"},
		ColumnTypes: []string{"i32", "text", "text"},
		Rows:        r.rows,
		Cost:        meter.Accrued,
	}, nil
}

// planExplainInner resolves the inner statement into a queryPlan WITHOUT executing it. Slice 1
// handles a read query (SELECT); DML and top-level set-op / WITH are later slices.
func (db *engine) planExplainInner(inner *statement) (queryPlan, error) {
	ptypes := &paramTypes{}
	switch {
	case inner.Select != nil:
		return db.planQuery(queryExpr{Select: inner.Select}, nil, nil, ptypes)
	default:
		return queryPlan{}, newError(FeatureNotSupported, "EXPLAIN of this statement is not yet supported")
	}
}

// renderQueryPlan walks a queryPlan arm at the given depth. Slice 1 handles a SELECT plan; set-op /
// VALUES / WITH arms are later slices.
func (db *engine) renderQueryPlan(r *explainRender, qp queryPlan, depth int) error {
	switch {
	case qp.sel != nil:
		return db.renderSelectPlan(r, qp.sel, depth)
	default:
		return newError(FeatureNotSupported, "EXPLAIN of this query shape is not yet supported")
	}
}

// renderSelectPlan emits a selectPlan's nodes in operator order (outermost first, each the pre-order
// parent of the next): Limit, Distinct, Sort (only when the sort is not elided), Filter, then the FROM
// tree. Slice 1 handles a single base-table scan; joins / aggregates / windows / non-base relations
// are later slices.
func (db *engine) renderSelectPlan(r *explainRender, sp *selectPlan, depth int) error {
	d := depth
	if sp.limit != nil || sp.offset != nil {
		r.emit(d, "Limit", limitDetail(sp.limit, sp.offset))
		d++
	}
	if sp.distinct {
		r.emit(d, "Distinct", "-")
		d++
	}
	if len(sp.order) > 0 && !sp.pkOrdered && sp.indexOrder == nil && !sp.joinPkOrdered {
		r.emit(d, "Sort", fmt.Sprintf("keys=%d", len(sp.order)))
		d++
	}
	if sp.filter != nil {
		r.emit(d, "Filter", fmt.Sprintf("conjuncts=%d", conjunctCount(sp.filter)))
		d++
	}
	return db.renderFrom(r, sp, d)
}

// renderFrom emits the FROM tree. Slice 1 supports exactly one base-table relation with no joins,
// aggregation, or window stage; every other shape is a later slice (0A000 until then).
func (db *engine) renderFrom(r *explainRender, sp *selectPlan, depth int) error {
	if sp.isAgg || sp.hasWindow || len(sp.rels) != 1 || len(sp.joins) != 0 {
		return newError(FeatureNotSupported, "EXPLAIN of this query shape is not yet supported")
	}
	rel := sp.rels[0]
	if rel.srf != nil || rel.cte != nil || rel.derived != nil {
		return newError(FeatureNotSupported, "EXPLAIN of this relation kind is not yet supported")
	}
	r.emit(depth, "Scan "+rel.tableName, db.scanDetail(rel.tableName, sp.relBounds[0], sp.relMasks[0]))
	return nil
}

// scanDetail renders a Scan node's attributes: the access path (from the relation's chosen scan
// bound, nil = a full scan), then the touched-column count when the query references any column.
func (db *engine) scanDetail(tableName string, b *scanBound, mask []bool) string {
	parts := []string{db.accessPath(tableName, b)}
	if n := countTrue(mask); n > 0 {
		parts = append(parts, fmt.Sprintf("touched=%d", n))
	}
	return strings.Join(parts, "; ")
}

// accessPath renders the chosen access path for a relation (spec/design/explain.md §5): a full scan,
// a primary-key range bound, or a secondary-index / GIN / GiST bound (the last three by index name).
func (db *engine) accessPath(tableName string, b *scanBound) string {
	switch {
	case b == nil:
		return "Full scan"
	case b.pk != nil:
		return "PK bound: " + renderBoundTerms(db.firstPKColName(tableName), b.pk.terms)
	case b.index != nil:
		return "Index bound: using " + b.index.nameKey
	case b.gin != nil:
		return "GIN bound: using " + b.gin.nameKey
	case b.gist != nil:
		return "GiST bound: using " + b.gist.nameKey
	default:
		return "Full scan"
	}
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
		return "col"
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
