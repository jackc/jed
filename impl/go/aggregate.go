package jed

import (
	"bytes"
	"fmt"
	"math"
	"sort"
	"strings"
)

// Aggregate accumulation and aggregate/window detection (spec/design/aggregates.md). This file holds
// the accumulator state machine (acc + fold/unfold/finalize, including ordered-set/hypothetical-set
// finalization and the percentile helpers), aggregate-name classification (isAggregateName and
// friends), the tree walks that detect whether a projection/ORDER BY contains an aggregate or window
// call (exprHasAggregate/exprHasWindow/itemsHaveWindow), and window-clause resolution + desugaring
// (resolveWindowClause/desugarItems/desugarNamedWindows). Function-call resolution is in
// aggregate_resolve.go.

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
	// groupKeyExprs is parallel to groupKeys: for each master grouping key, a non-nil *groupKeyExpr
	// (canonical AST + resolved type) if it is a general EXPRESSION key (`GROUP BY a + b`,
	// aggregates.md §15) — so a projection / HAVING / ORDER BY expression that structurally matches it
	// resolves to that group's synthetic slot — or nil for a plain COLUMN key (matched by the column
	// path instead).
	groupKeyExprs []*groupKeyExpr
	specs         []aggSpec
	// windowing marks a non-aggregate WINDOW query's projection (spec/design/window.md §5.1):
	// bare columns resolve to the real input row (like the Forbidden mode), and a FuncCall carrying
	// an OVER clause collects into windowSpecs and resolves to the synthetic slot
	// windowBase+window_index — the window stage appends each function's result after the input
	// columns. S0 narrows window + aggregate/GROUP BY to 0A000, so collecting and windowing are
	// never both set.
	windowing   bool
	windowBase  int
	windowSpecs []windowSpec
	// windowKeys collects the materialized window-key expressions (a non-column PARTITION BY / ORDER
	// BY key — `PARTITION BY a + b`, or `ORDER BY sum(x) + 1`), each resolved to the placeholder slot
	// windowKeyBase+k. A bare column or a bare aggregate (`ORDER BY sum(x)`) resolves to its real row
	// slot and is NOT materialized (spec/design/window.md §5.1).
	windowKeys []*rExpr
	// groupingSpecs collects one entry per GROUPING(c1,…,ck) call from the projection / HAVING — the
	// master-grouping-column POSITIONS (indices into groupKeys) of its arguments. Each call resolves
	// to the placeholder slot groupingGsBase+index, rebased after resolution to its real trailing
	// synthetic slot (spec/design/aggregates.md §12).
	groupingSpecs [][]int
}

// windowSpec is one resolved window function (spec/design/window.md §5.1): its plan, the resolved
// PARTITION BY key column slots (flat input-row indices), and the resolved within-partition ORDER
// BY (sort keys over the input row, PK tie-break applied by the stable sort over the PK-ordered
// scan).
type windowSpec struct {
	plan      windowPlan
	partition []int
	order     []orderSlot
	// args holds the resolved function arguments (empty for the no-argument ranking functions;
	// ntile's bucket count is one integer argument, evaluated once per partition). Future
	// offset/value functions (lag/lead/nth_value, S2+) carry their value/offset/default here.
	// For an aggregate window (plan == planAgg) the aggregate operand (if any) is args[0];
	// COUNT(*) has empty args.
	args []*rExpr
	// aggPlan is the aggregate runtime plan when plan == planAgg (S3 — sum/count/min/max/avg
	// OVER (...)). Go's windowPlan is an int enum and cannot carry a payload like Rust's
	// WindowPlan::Agg(AggPlan) tuple variant, so the aggregate plan rides alongside here.
	aggPlan aggPlan
	// frame is the resolved explicit frame, or nil for the default frame (RANGE UNBOUNDED PRECEDING
	// TO CURRENT ROW with an ORDER BY, the whole partition without — window.md §6). Mirrors Rust's
	// WindowSpec.frame.
	frame *resolvedFrame
	// filter is agg(x) FILTER (WHERE cond) OVER (…) — a per-frame-row boolean restricting which frame
	// rows fold into the window aggregate (aggregates.md §20). Non-nil only for an aggregate window
	// function (a non-aggregate window function with FILTER is 0A000). A FILTER disables the
	// sliding-frame optimization (a filtered row can't be cleanly un-folded) — every frame re-folds.
	filter *rExpr
}

// resolvedFrame is a resolved window frame (spec/design/window.md §6). ROWS physical offsets,
// GROUPS peer-group offsets (both integer counts), and RANGE value offsets over the ordering key.
type resolvedFrame struct {
	mode    frameMode
	start   resolvedBound
	end     resolvedBound
	exclude frameExclusion // EXCLUDE … — rows dropped from [lo, hi) per current row (window.md §6)
}

// resolvedBoundKind distinguishes the five resolved frame-boundary forms.
type resolvedBoundKind int

const (
	boundUnboundedPreceding resolvedBoundKind = iota
	boundPreceding                            // offset before the current row; offVal carries it
	boundCurrentRow
	boundFollowing // offset after the current row; offVal carries it
	boundUnboundedFollowing
)

// resolvedBound is one resolved frame boundary; offVal carries the non-negative offset for
// boundPreceding / boundFollowing (unused otherwise) — a ValInt row/group count for ROWS/GROUPS,
// or the numeric Value (Int over an integer key, Decimal over a decimal key) for RANGE.
type resolvedBound struct {
	kind   resolvedBoundKind
	offVal Value
}

// windowPlan is the runtime plan for one window function (spec/design/window.md §4). S0:
// row_number only; ranking / offset / aggregate-window / frame plans land in S1–S4.
type windowPlan int

const (
	// planRowNumber — ROW_NUMBER(): the 1-based sequence position within the partition.
	planRowNumber windowPlan = iota
	// planRank — RANK(): 1 + the number of rows in earlier peer groups (ties share a rank, gap).
	planRank
	// planDenseRank — DENSE_RANK(): 1 + the number of earlier peer groups (ties share, no gap).
	planDenseRank
	// planPercentRank — PERCENT_RANK(): (rank-1)/(N-1), 0 when N=1; f64 (PG's float8, window.md §4).
	planPercentRank
	// planCumeDist — CUME_DIST(): (rows through the current peer group)/N; f64 (PG's float8).
	planCumeDist
	// planNtile — NTILE(n): distribute the partition into n ranked buckets (larger first),
	// numbered 1..n. Position-based (not peer-based); n <= 0 → 22014; NULL n → NULL for every row.
	planNtile
	// planLag — LAG(value [, offset [, default]]): value `offset` rows EARLIER in the partition
	// (default offset 1); out-of-range → default (or NULL). Frame-insensitive (sorted position).
	planLag
	// planLead — LEAD(value [, offset [, default]]): value `offset` rows LATER in the partition;
	// otherwise identical to LAG (the offset direction flips).
	planLead
	// planAgg — an aggregate used as a window function (S3): sum/count/min/max/avg(...) OVER (...),
	// folded over the row's default frame (running with a window ORDER BY, whole-partition without)
	// or an explicit frame (S4 — spec/design/window.md §6). Reuses the aggregate `acc` kernels; the
	// aggregate plan is held in windowSpec.aggPlan and the operand (if any) is args[0]. Mirrors
	// Rust's WindowPlan::Agg.
	planAgg
	// planFirstValue — FIRST_VALUE(v): the value of the frame's first row (S4). args[0] is the
	// value expression; frame-sensitive. Mirrors Rust's WindowPlan::FirstValue.
	planFirstValue
	// planLastValue — LAST_VALUE(v): the value of the frame's last row (S4). args[0] is the value
	// expression; frame-sensitive. Mirrors Rust's WindowPlan::LastValue.
	planLastValue
	// planNthValue — NTH_VALUE(v, n): the value of the frame's n-th row, NULL if the frame has < n
	// rows (S4). args[0] is the value, args[1] the position; frame-sensitive. Mirrors Rust's
	// WindowPlan::NthValue.
	planNthValue
)

// aggPlan is the runtime plan for one aggregate, fixed at resolve from the function + operand
// type (the PG widening — spec/design/aggregates.md §3).
type aggPlan int

const (
	planCountStar  aggPlan = iota // COUNT(*) — count every row
	planCount                     // COUNT(expr) — count non-NULL inputs
	planSumInt                    // SUM(i16|i32) — accumulate i64, result i64 (trap at i64)
	planSumDecimal                // SUM(i64|decimal) — accumulate decimal, result decimal
	planAvg                       // AVG — decimal sum + i64 count; result sum/count (NULL if 0)
	planSumFloat32                // SUM(f32) — canonical-order fold, result f32 (float.md §7)
	planSumFloat64                // SUM(f64) — canonical-order fold, result f64
	planAvgFloat32                // AVG(f32) — fold sum / count, result f32
	planAvgFloat64                // AVG(f64) — fold sum / count, result f64
	planMin
	planMax
	planJsonbAgg       // jsonb_agg(x) — aggregate the JSON images into a jsonb array
	planJsonAgg        // json_agg(x) — same array, typed json (spaced canonical render)
	planJsonbAggStrict // jsonb_agg_strict(x) — skip NULL-valued rows; jsonb array
	planJsonAggStrict  // json_agg_strict(x) — skip NULL-valued rows; json array
	// json[b]_object_agg[_unique] (B4) — aggregate (key, value) pairs (a Row operand) into a JSON
	// object (json-sql-functions.md §4). The plan encodes the json-vs-jsonb render and the _unique
	// flag; the operand is a 2-field Row(key, value) the fold splits back out.
	planJsonbObjectAgg       // jsonb_object_agg(k, v) — canonical (last-wins dedup, key sort, spaced)
	planJsonObjectAgg        // json_object_agg(k, v) — row order + dups, '{ … }' brace-padded spacing
	planJsonbObjectAggUnique // jsonb_object_agg_unique(k, v) — as jsonb, but 22030 on a duplicate key
	planJsonObjectAggUnique  // json_object_agg_unique(k, v) — as json, but 22030 on a duplicate key
	// Ordered-set aggregates (spec/design/aggregates.md §13). The WITHIN GROUP direction +
	// percentile fraction live on the aggSpec/acc, not the plan.
	planMode           // mode() — the most frequent value (tie → first in sort order)
	planPercentileDisc // percentile_disc(f) — the discrete percentile (an actual input value)
	planPercentileCont // percentile_cont(f) — the continuous (interpolated) percentile, f64
	// percentile_cont(f) over an interval input — the continuous percentile interpolated in the
	// interval domain (lo + (hi-lo)·pct, PG interval_lerp); result interval (aggregates.md §13).
	// Values buffered as ValInterval in osaVals (the non-cont branch).
	planPercentileContInterval
	// Hypothetical-set aggregates (spec/design/aggregates.md §19): rank/dense_rank/percent_rank/
	// cume_dist used WITH a WITHIN GROUP clause (these names are ALSO window functions; the WITHIN
	// GROUP clause routes them here). The hypothetical-row direct args + the WITHIN GROUP key
	// operands + per-key sort specs live on the aggSpec's hypo field, not the plan.
	planHypoRank        // rank(args) — 1 + the number of group rows that sort strictly before; result i64
	planHypoDenseRank   // dense_rank(args) — 1 + the number of DISTINCT values strictly before; result i64
	planHypoPercentRank // percent_rank(args) — (rank − 1) / N; result f64
	planHypoCumeDist    // cume_dist(args) — (#rows ≤ hyp + 1) / (N + 1); result f64
)

// aggSpec is one resolved aggregate: its plan and its resolved argument (evaluated per input
// row against the real row). operand is nil for COUNT(*). distinct (COUNT(DISTINCT x),
// aggregates.md §5) folds only the distinct non-NULL argument values — the fold loop keeps a
// per-group value-canonical set and skips a value already seen. Only set in the aggregation
// stage; a window aggregate is never DISTINCT (0A000, rejected at resolve).
type aggSpec struct {
	plan     aggPlan
	operand  *rExpr
	distinct bool
	// filter is the resolved FILTER (WHERE cond) boolean predicate (SUM(x) FILTER (WHERE cond) —
	// aggregates.md §11); nil for an unfiltered aggregate. The fold loop evaluates it per input row
	// and folds only the rows for which it is TRUE (so the filter applies before the DISTINCT dedup).
	filter *rExpr
	// osaDesc / osaFrac are the ordered-set aggregate parameters (mode/percentile_* —
	// aggregates.md §13/§17), set only for the planMode/planPercentile* plans. osaDesc is the
	// WITHIN GROUP sort direction; osaFrac is the resolved **direct argument** (the percentile
	// fraction) — resolved in the grouped context so it references grouping columns by their
	// synthetic key slots (a non-grouped column is 42803, matching PG's "direct arguments … must
	// use only grouped columns") and is evaluated **per group** at finalize against the synthetic
	// row. nil for mode (no direct argument).
	osaDesc bool
	osaFrac *rExpr
	// osaCollation is the WITHIN GROUP key's collation (aggregates.md §13) — non-nil for an explicit
	// COLLATE on the key or a column's frozen non-C collation, nil for the default byte (C) order. The
	// finalize sort applies it to the buffered text values.
	osaCollation *Collation
	// hypo is the hypothetical-set aggregate parameters (rank/dense_rank/percent_rank/cume_dist
	// WITHIN GROUP — aggregates.md §19), set only for the planHypo* plans, nil otherwise. (operand
	// is nil here — the keys are buffered as a tuple per row from hypo.keys.)
	hypo *hypoParams
}

// keySort is a single WITHIN GROUP ordering-key sort spec (aggregates.md §13/§19): direction, NULL
// placement, and an optional collation (text keys only).
type keySort struct {
	desc       bool
	nullsFirst bool
	collation  *Collation
}

// hypoParams are the resolve-time parameters of a hypothetical-set aggregate (aggregates.md §19).
// args are the hypothetical-row direct arguments (evaluated PER GROUP at finalize, like an OSA
// fraction — they may reference grouping columns); keys are the WITHIN GROUP key operands
// (evaluated PER ROW during the fold and buffered as a tuple); sorts is the per-key ordering spec.
// The three slices have equal length (the arity check at resolve).
type hypoParams struct {
	args  []*rExpr
	keys  []*rExpr
	sorts []keySort
}

// acc is a running aggregate accumulator (one per aggSpec), folded per input row then finalized.
type acc struct {
	plan     aggPlan
	count    int64
	sumInt   int64
	sumDec   Decimal
	seen     bool
	cur      Value
	hasCur   bool
	floatSum *floatSumAcc // non-nil for the float SUM/AVG plans (the streaming scan-order fold — float.md §7)
	// json_agg / jsonb_agg accumulator (B4): the inputs' JSON-image nodes in row order. jsonAsJSON
	// selects the `json` result type (vs jsonb); jsonStrict skips a NULL-valued row. `seen` is reused
	// to mark the group non-empty even when the strict filter drops every row (empty group → NULL,
	// all-skipped group → `[]`).
	jsonNodes  []JsonNode
	jsonAsJSON bool
	jsonStrict bool
	// json[b]_object_agg[_unique] accumulator (B4): the (key, value) pairs in row order. objAgg is
	// true for the object-agg plans; objUnique errors 22030 on a duplicate key. jsonAsJSON selects
	// the json (brace-padded, row order, dups) vs jsonb (canonical, last-wins) finalize render, and
	// `seen` distinguishes a zero-row group (→ NULL) from a non-empty one (→ an object).
	objPairs  []objAggPair
	objAgg    bool
	objUnique bool
	// Ordered-set aggregate state (spec/design/aggregates.md §13): the WITHIN GROUP direction +
	// the **evaluated** percentile fraction for this group, plus the collected non-NULL values
	// (percentile_cont widens to f64 into osaFloats; mode/percentile_disc keep the Value in
	// osaVals). osaFrac is the per-group fraction Value — the direct argument is evaluated per
	// group against the synthetic row just before finalize (aggregates.md §13/§17): non-nil for
	// percentile_* (the Value may be NULL → NULL result, or numeric), nil for mode. Sorted +
	// computed at finalize.
	osaDesc bool
	osaFrac *Value
	// osaCollation is the WITHIN GROUP key collation (aggregates.md §13) applied to the finalize sort
	// of the buffered text values; nil is the default byte (C) order.
	osaCollation *Collation
	osaVals      []Value
	osaFloats    []float64
	// hypoRows buffers every row's WITHIN GROUP key TUPLE for a hypothetical-set aggregate
	// (rank/dense_rank/percent_rank/cume_dist — aggregates.md §19). The fold loop appends each tuple
	// (no NULL-skip — every row counts); at finalize (in the group-emission loop, where the per-group
	// hypothetical row + the spec's sort specs are available) finalizeHypothetical counts how that
	// hypothetical row would rank. plan selects the result formula.
	hypoRows [][]Value
}

// objAggPair is one (key, value) pair accumulated by json[b]_object_agg (the key already coerced to
// text, the value its raw Value carried to finalize where it becomes its to_jsonb / json image).
type objAggPair struct {
	key string
	val Value
}

func newAcc(plan aggPlan) *acc {
	a := &acc{plan: plan}
	if plan == planSumDecimal || plan == planAvg {
		a.sumDec = decimalFromInt64(0)
	}
	switch plan {
	case planSumFloat32, planAvgFloat32:
		a.floatSum = newFloatSumAcc(true)
	case planSumFloat64, planAvgFloat64:
		a.floatSum = newFloatSumAcc(false)
	case planJsonbAgg:
	case planJsonAgg:
		a.jsonAsJSON = true
	case planJsonbAggStrict:
		a.jsonStrict = true
	case planJsonAggStrict:
		a.jsonAsJSON, a.jsonStrict = true, true
	case planJsonbObjectAgg:
		a.objAgg = true
	case planJsonObjectAgg:
		a.objAgg, a.jsonAsJSON = true, true
	case planJsonbObjectAggUnique:
		a.objAgg, a.objUnique = true, true
	case planJsonObjectAggUnique:
		a.objAgg, a.jsonAsJSON, a.objUnique = true, true, true
	}
	return a
}

// newAccFromSpec builds the accumulator for one resolved aggregate. Ordered-set aggregates
// (mode/percentile_* — aggregates.md §13) need their WITHIN GROUP direction + fraction (carried on
// the spec, not the plan); every other plan delegates to newAcc. Only the aggregation stage builds
// ordered-set accumulators — the window stage (which calls newAcc directly) never sees one (an
// ordered-set aggregate with OVER is 0A000, rejected at resolve).
func newAccFromSpec(s aggSpec) *acc {
	switch s.plan {
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		// The per-group fraction (osaFrac) is filled in just before finalize (the direct argument
		// evaluated against the synthetic row); mode keeps it nil.
		return &acc{plan: s.plan, osaDesc: s.osaDesc, osaCollation: s.osaCollation}
	case planHypoRank, planHypoDenseRank, planHypoPercentRank, planHypoCumeDist:
		// A hypothetical-set aggregate buffers each row's WITHIN GROUP key tuple (aggregates.md §19);
		// it is finalized inline in the group-emission loop (it needs the spec's sort specs).
		return &acc{plan: s.plan}
	default:
		return newAcc(s.plan)
	}
}

// clone returns an independent snapshot of the running accumulator, so the window stage can
// finalize a peer-group's cumulative value without consuming the still-running acc (Rust's
// `acc.clone().finalize()`). The acc struct copies by value, but floatSum is a POINTER whose
// `finite` slice would otherwise alias the original — deep-copy it (the only shared-reference
// field; `cur` holds a Value finalize only reads, never mutates). Mirrors Rust's #[derive(Clone)]
// on Acc with its deep slice clone.
func (a *acc) clone() *acc {
	c := *a // value copy (count/sumInt/sumDec/seen/cur/hasCur are independent)
	if a.floatSum != nil {
		// floatSum is a streaming running total (no slice) — a plain value copy is independent.
		fs := *a.floatSum
		c.floatSum = &fs
	}
	if a.jsonNodes != nil {
		// Deep-copy the node slice so a window peer-group's finalize doesn't consume the running
		// acc's nodes (mirrors the floatSum slice clone above).
		c.jsonNodes = append([]JsonNode(nil), a.jsonNodes...)
	}
	if a.objPairs != nil {
		// Deep-copy the (key, value) pairs slice (as jsonNodes above) so a cloned-finalize over a
		// window peer-group doesn't alias the still-running acc.
		c.objPairs = append([]objAggPair(nil), a.objPairs...)
	}
	// Ordered-set accumulators are never windowed (clone is the window-stage snapshot), but deep-copy
	// the collected slices anyway so a clone never aliases the original.
	if a.osaVals != nil {
		c.osaVals = append([]Value(nil), a.osaVals...)
	}
	if a.osaFloats != nil {
		c.osaFloats = append([]float64(nil), a.osaFloats...)
	}
	// Hypothetical-set accumulators are never windowed (clone is the window-stage snapshot), but
	// deep-copy the buffered tuples anyway so a clone never aliases the original.
	if a.hypoRows != nil {
		c.hypoRows = make([][]Value, len(a.hypoRows))
		for i := range a.hypoRows {
			c.hypoRows[i] = append([]Value(nil), a.hypoRows[i]...)
		}
	}
	return &c
}

// fold folds one input value into the accumulator. NULL arguments are skipped (COUNT(*)
// ignores the value and always counts). Traps 22003 on SUM/AVG overflow at the result bound.
// A decimal SUM/AVG fold charges size-scaled decimal_work against the running accumulator
// (the `+` formula — spec/design/cost.md §3 "decimal_work"); MIN/MAX folds are direct Value
// compares like the sort's and stay unmetered.
func (a *acc) fold(v Value, m *costMeter) error {
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
				return overflowErr(scalarInt64)
			}
			a.sumInt = s
			a.seen = true
		}
	case planSumDecimal:
		if !v.IsNull() {
			in := toDecimal(v)
			m.Charge(costs.DecimalWork * (workLinear(a.sumDec, in) - 1))
			if err := m.Guard(); err != nil {
				return err
			}
			// Uncapped: the running sum may exceed the §2 format cap mid-fold; only the FINAL
			// result is cap-checked (in finalize), matching PG and making the trap
			// order-independent (spec/design/decimal.md §2, determinism.md §7).
			a.sumDec = a.sumDec.AddUncapped(in)
			a.seen = true
		}
	case planAvg:
		if !v.IsNull() {
			in := toDecimal(v)
			m.Charge(costs.DecimalWork * (workLinear(a.sumDec, in) - 1))
			if err := m.Guard(); err != nil {
				return err
			}
			// Uncapped (as planSumDecimal): the average's final divide brings the value back in
			// range, so AVG never traps on an over-cap intermediate sum the way PG does not.
			a.sumDec = a.sumDec.AddUncapped(in)
			a.count++
		}
	case planSumFloat32, planSumFloat64, planAvgFloat32, planAvgFloat64:
		// Float SUM/AVG fold into the streaming running total in scan order; NULLs are skipped. The
		// fold order is ledgered non-deterministic (spec/design/float.md §7). The per-row
		// aggregate_accumulate is charged by the caller, so this stays O(1)/row and O(1) memory.
		if !v.IsNull() {
			a.floatSum.add(v)
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
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict:
		// Mark the group non-empty even when the strict filter drops this row (an all-skipped group
		// still finalizes to `[]`, not NULL — only a zero-row group is NULL).
		a.seen = true
		// Non-strict: a NULL input contributes a JSON null; `_strict` skips it. Each input's JSON
		// image is the to_jsonb kernel (deferred 0A000 sources propagate here). One generated_row
		// per appended element.
		if !(a.jsonStrict && v.IsNull()) {
			m.Charge(costs.GeneratedRow)
			if err := m.Guard(); err != nil {
				return err
			}
			node, err := valueToNode(v)
			if err != nil {
				return err
			}
			a.jsonNodes = append(a.jsonNodes, node)
		}
	case planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique:
		// The operand is a Row(key, value) composite; mark the group non-empty (an empty group → NULL,
		// not `{}`) and split the two fields back out.
		a.seen = true
		m.Charge(costs.GeneratedRow)
		if err := m.Guard(); err != nil {
			return err
		}
		if v.Kind != ValComposite || v.composite() == nil || len(*v.composite()) != 2 {
			panic("BUG: object_agg operand is a 2-field Row")
		}
		fields := *v.composite()
		kv, vv := fields[0], fields[1]
		// The key coerces to text (text/integer/decimal/boolean); a NULL key → 22023, but with a
		// DIFFERENT message from build_object's "key must not be null" (NULL handled here, before
		// objectKeyText, so the non-NULL coercion + the non-scalar 0A000 still reuse it).
		if kv.Kind == ValNull {
			return newError(InvalidParameterValue, "field name must not be null")
		}
		key, err := objectKeyText(kv, 1)
		if err != nil {
			return err
		}
		if a.objUnique {
			for i := range a.objPairs {
				if a.objPairs[i].key == key {
					return newError(DuplicateJsonObjectKeyValue, "duplicate JSON object key value")
				}
			}
		}
		a.objPairs = append(a.objPairs, objAggPair{key: key, val: vv})
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		// Collect the non-NULL aggregated argument (the WITHIN GROUP order key, evaluated per row).
		// percentile_cont (numeric) widens each value to f64 up front (the correctly-rounded cast,
		// matching PG's numeric→float8); mode/percentile_disc and percentile_cont over interval keep
		// the Value (the latter interpolates in the interval domain at finalize).
		if !v.IsNull() {
			if a.plan == planPercentileCont {
				f, err := percentileInputF64(v)
				if err != nil {
					return err
				}
				a.osaFloats = append(a.osaFloats, f)
			} else {
				a.osaVals = append(a.osaVals, v)
			}
		}
	case planHypoRank, planHypoDenseRank, planHypoPercentRank, planHypoCumeDist:
		// A hypothetical-set aggregate buffers its key tuple in the fold LOOP (which has the row),
		// not through acc.fold (aggregates.md §19), so this is never reached.
		panic("a hypothetical-set accumulator buffers tuples in the fold loop")
	}
	return nil
}

// unfold removes one input value — the inverse of fold — used ONLY by the sliding-window
// optimization (window.md §5.2/§8) for the exactly-invertible COUNT / COUNT(*) (integer counters:
// add-then-remove is exact and order-independent). Every other accumulator is never un-folded — a
// moving frame over SUM/AVG/MIN/MAX/float re-folds from scratch instead (decimal scale,
// intermediate-overflow trap order, and float non-associativity make them unsafe to invert).
func (a *acc) unfold(v Value, _ *costMeter) {
	switch a.plan {
	case planCountStar:
		a.count--
	case planCount:
		if !v.IsNull() {
			a.count--
		}
	default:
		panic("only COUNT/COUNT(*) are un-folded by the sliding-window optimization")
	}
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
			// The only cap check for the fold: the FINAL sum traps 22003 if over the §2 cap
			// (PG's make_result), but no intermediate does (decimal.md §2).
			d, err := a.sumDec.CheckCap()
			if err != nil {
				return NullValue(), err
			}
			return DecimalValue(d), nil
		}
		return NullValue(), nil
	case planAvg:
		if a.count == 0 {
			return NullValue(), nil
		}
		// Div cap-checks its (in-range) result; the over-cap-capable running sum is never
		// surfaced directly, so AVG matches PG even when SUM would overflow.
		d, err := a.sumDec.Div(decimalFromInt64(a.count))
		if err != nil {
			return NullValue(), err
		}
		return DecimalValue(d), nil
	case planSumFloat32:
		f, ok, err := a.floatSum.sumF32()
		if err != nil || !ok {
			return NullValue(), err
		}
		return Float32Value(f), nil
	case planSumFloat64:
		f, ok, err := a.floatSum.sumF64()
		if err != nil || !ok {
			return NullValue(), err
		}
		return Float64Value(f), nil
	case planAvgFloat32:
		f, ok, err := a.floatSum.avgF32()
		if err != nil || !ok {
			return NullValue(), err
		}
		return Float32Value(f), nil
	case planAvgFloat64:
		f, ok, err := a.floatSum.avgF64()
		if err != nil || !ok {
			return NullValue(), err
		}
		return Float64Value(f), nil
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict:
		// json_agg/jsonb_agg: NULL over an empty (zero-row) group; else the JSON array. A non-empty
		// group the strict filter emptied still finalizes to `[]` (seen is true, jsonNodes empty).
		if !a.seen {
			return NullValue(), nil
		}
		arr := JsonNode{Kind: JArray, Arr: a.jsonNodes}
		// Both json_agg and jsonb_agg render the SPACED canonical form (PG joins the element texts
		// with ", "); the json variant is just typed `json` carrying that same text. (A json input
		// element is canonicalized by valueToNode — a documented divergence from PG's verbatim.)
		if a.jsonAsJSON {
			return JsonValue(jsonbOut(&arr)), nil
		}
		return JsonbValue(arr), nil
	case planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique:
		// json[b]_object_agg: NULL over an empty (zero-row) group; else the JSON object. json keeps
		// the group's row order + duplicate keys and PG's '{ … }' brace-padded ', ' / ' : ' spacing;
		// jsonb canonicalizes (last-wins dedup + canonical key sort) via makeObject.
		if !a.seen {
			return NullValue(), nil
		}
		if a.jsonAsJSON {
			parts := make([]string, 0, len(a.objPairs))
			for _, p := range a.objPairs {
				// The key's json-quoted form (jsonCompactOut of a JSON string node), then ` : `, then
				// the value's json image (json verbatim / jsonb canonical-spaced / else compact).
				img, err := elemJsonText(p.val)
				if err != nil {
					return NullValue(), err
				}
				keyNode := JsonNode{Kind: JString, S: p.key}
				parts = append(parts, jsonCompactOut(&keyNode)+" : "+img)
			}
			// PG's json_object_agg PADS the braces (`{ … }`) — distinct from json_build_object, which
			// does NOT pad.
			return JsonValue("{ " + strings.Join(parts, ", ") + " }"), nil
		}
		members := make([]JsonMember, 0, len(a.objPairs))
		for _, p := range a.objPairs {
			node, err := valueToNode(p.val)
			if err != nil {
				return NullValue(), err
			}
			members = append(members, JsonMember{Key: p.key, Val: node})
		}
		return JsonbValue(makeObject(members)), nil
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		return a.finalizeOrderedSet()
	case planHypoRank, planHypoDenseRank, planHypoPercentRank, planHypoCumeDist:
		// A hypothetical-set aggregate is finalized in the group-emission loop (it needs the spec's
		// per-key sort specs), never through acc.finalize (aggregates.md §19).
		panic("a hypothetical-set accumulator is finalized in the group-emission loop")
	default: // planMin, planMax
		if a.hasCur {
			return a.cur, nil
		}
		return NullValue(), nil
	}
}

// finalizeOrderedSet computes an ordered-set aggregate's value over its collected group
// (spec/design/aggregates.md §13). mode → the most frequent value (tie → first in WITHIN GROUP sort
// order); percentile_disc → an actual value at 1-based row ceil(p·N); percentile_cont → the
// interpolated f64. The fraction range check (22003) fires here, after the NULL-fraction check and
// before the empty-group check — matching PG.
func (a *acc) finalizeOrderedSet() (Value, error) {
	desc := a.osaDesc
	switch a.plan {
	case planMode:
		vals := a.osaVals
		if len(vals) == 0 {
			return NullValue(), nil
		}
		// Sort by the WITHIN GROUP order (honoring the key's collation), then take the first value of
		// the longest run of equal values. Run equality is value-canonical (byte equality), so the
		// collation affects only which tied value comes first.
		if err := sortOsaVals(vals, a.osaCollation, desc); err != nil {
			return NullValue(), err
		}
		// The first value of the longest run of equal values — the most frequent, ties broken by
		// sort order (the first such run).
		bestIdx, bestCount, runStart := 0, 1, 0
		for i := 1; i < len(vals); i++ {
			if valueCmp(vals[i], vals[runStart]) == 0 {
				if runLen := i - runStart + 1; runLen > bestCount {
					bestCount, bestIdx = runLen, runStart
				}
			} else {
				runStart = i
			}
		}
		return vals[bestIdx], nil
	case planPercentileDisc:
		// percentile_disc: an actual sorted value at row ceil(p·N). The fraction may be a scalar or
		// an array (aggregates.md §18); finalizePercentile dispatches and applies the NULL /
		// range-check / empty rules per PG, computing each percentile over the sorted vals.
		vals := a.osaVals
		if err := sortOsaVals(vals, a.osaCollation, desc); err != nil {
			return NullValue(), err
		}
		return finalizePercentile(a.osaFrac, len(vals) == 0, func(p float64) (Value, error) {
			return percentileDiscAt(vals, p), nil
		})
	case planPercentileCont:
		fs := a.osaFloats
		sort.SliceStable(fs, func(i, j int) bool {
			return dirCmp(floatTotalCmp(fs[i], fs[j]), desc) < 0
		})
		return finalizePercentile(a.osaFrac, len(fs) == 0, func(p float64) (Value, error) {
			return Float64Value(percentileContAt(fs, p)), nil
		})
	case planPercentileContInterval:
		// percentile_cont over interval input: interpolate in the interval domain (PG interval_lerp
		// — aggregates.md §13). Values are sorted by their canonical span (interval has no collation,
		// so sortOsaVals uses the value order).
		vals := a.osaVals
		if err := sortOsaVals(vals, a.osaCollation, desc); err != nil {
			return NullValue(), err
		}
		return finalizePercentile(a.osaFrac, len(vals) == 0, func(p float64) (Value, error) {
			n := len(vals)
			pos := p * float64(n-1)
			first := int(math.Floor(pos))
			second := int(math.Ceil(pos))
			lo := expectInterval(vals[first])
			if first == second {
				return IntervalValue(lo), nil
			}
			hi := expectInterval(vals[second])
			r, err := intervalLerp(lo, hi, pos-float64(first))
			if err != nil {
				return NullValue(), err
			}
			return IntervalValue(r), nil
		})
	default:
		panic("finalizeOrderedSet called for a non-ordered-set plan")
	}
}

// finalizePercentile applies the percentile fraction (scalar or array) to a sorted group, computing
// each percentile via compute (spec/design/aggregates.md §13/§18). PG's check order is preserved: a
// scalar None/NULL fraction → NULL; otherwise the range check (22003) fires per fraction BEFORE the
// empty-group check; an empty/all-NULL group → NULL (the whole result, even for an array). For an
// array fraction the result is an array with one percentile per element (a NULL element → a NULL
// element), after every non-NULL element has passed the range check.
func finalizePercentile(frac *Value, empty bool, compute func(p float64) (Value, error)) (Value, error) {
	if frac == nil || frac.IsNull() {
		return NullValue(), nil
	}
	if frac.Kind == ValArray {
		// Range-check every non-NULL element FIRST (before the empty-group check, PG).
		fracs := make([]*float64, 0, len(frac.arrayVal().Elements))
		for i := range frac.arrayVal().Elements {
			el := frac.arrayVal().Elements[i]
			pf, err := fractionToF64(&el)
			if err != nil {
				return NullValue(), err
			}
			if pf != nil {
				if err := checkPercentileFraction(*pf); err != nil {
					return NullValue(), err
				}
			}
			fracs = append(fracs, pf)
		}
		if empty {
			return NullValue(), nil // an empty/all-NULL group → NULL (not an array of NULLs), PG
		}
		out := make([]Value, 0, len(fracs))
		for _, pf := range fracs {
			if pf == nil {
				out = append(out, NullValue())
				continue
			}
			v, err := compute(*pf)
			if err != nil {
				return NullValue(), err
			}
			out = append(out, v)
		}
		return arrayValueOf(&ArrayVal{Dims: []int{len(out)}, Lbounds: []int32{1}, Elements: out}), nil
	}
	p, err := fractionToF64(frac)
	if err != nil {
		return NullValue(), err
	}
	if p == nil {
		return NullValue(), nil
	}
	if err := checkPercentileFraction(*p); err != nil {
		return NullValue(), err
	}
	if empty {
		return NullValue(), nil
	}
	return compute(*p)
}

// expectInterval returns the Interval of a buffered ValInterval (a planPercentileContInterval group
// only ever buffers intervals — the resolver gates the operand to interval).
func expectInterval(v Value) Interval {
	if v.Kind != ValInterval {
		panic("percentile_cont(interval) buffered a non-interval")
	}
	return v.interval()
}

// intervalLerp(lo, hi, pct) = lo + (hi - lo)·pct, PG's orderedsetaggs.c interval interpolation
// (spec/design/aggregates.md §13). intervalMul below replicates PG's exact field-cascade + rounding
// so the result is byte-identical to PostgreSQL.
func intervalLerp(lo, hi Interval, pct float64) (Interval, error) {
	diff, err := hi.Sub(lo)
	if err != nil {
		return Interval{}, err
	}
	scaled, err := intervalMul(diff, pct)
	if err != nil {
		return Interval{}, err
	}
	return scaled.Add(lo)
}

// intervalMul is interval * f64, byte-identical to PostgreSQL's interval_mul (timestamp.c): multiply
// each field by the factor, then cascade the fractional month/day parts down to days/micros with
// PG's TSROUND (round to microsecond precision) and the 30 days/month, 86400 s/day conversions. The
// operand is finite (no infinite intervals here) and the factor is a finite fraction in [0,1].
func intervalMul(span Interval, factor float64) (Interval, error) {
	const (
		daysPerMonthF = 30.0
		secsPerDayF   = 86400.0
		usecsPerSecF  = 1_000_000.0
	)
	// TSROUND: round to microsecond precision (PG TS_PREC_INV = 1e6). PG rint = ties-to-EVEN.
	tsround := func(j float64) float64 { return math.RoundToEven(j*usecsPerSecF) / usecsPerSecF }
	oor := func() error { return newError(DatetimeFieldOverflow, "interval out of range") }
	// FLOAT8_FITS_IN_INT32/64: x in [INT_MIN, -INT_MIN) — matches Rust's fits_i32/fits_i64.
	fitsI32 := func(x float64) bool { return x >= float64(math.MinInt32) && x < -float64(math.MinInt32) }
	fitsI64 := func(x float64) bool { return x >= float64(math.MinInt64) && x < -float64(math.MinInt64) }

	origMonth := span.Months
	origDay := span.Days

	resultDouble := float64(span.Months) * factor
	if math.IsNaN(resultDouble) || !fitsI32(resultDouble) {
		return Interval{}, oor()
	}
	resultMonth := int32(resultDouble)

	resultDouble = float64(span.Days) * factor
	if math.IsNaN(resultDouble) || !fitsI32(resultDouble) {
		return Interval{}, oor()
	}
	resultDay := int32(resultDouble)

	// Cascade fractional months → days, fractional days → micros (PG's exact sequence).
	monthRemainderDays := tsround((float64(origMonth)*factor - float64(resultMonth)) * daysPerMonthF)
	secRemainder := tsround(
		(float64(origDay)*factor - float64(resultDay) + monthRemainderDays -
			float64(int64(monthRemainderDays))) * secsPerDayF,
	)
	// Might exceed a day from rounding / cascade — push whole days up.
	if math.Abs(secRemainder) >= secsPerDayF {
		add := int32(secRemainder / secsPerDayF)
		nd, ok := addI32(resultDay, add)
		if !ok {
			return Interval{}, oor()
		}
		resultDay = nd
		secRemainder -= float64(int64(secRemainder/secsPerDayF)) * secsPerDayF
	}
	nd, ok := addI32(resultDay, int32(monthRemainderDays))
	if !ok {
		return Interval{}, oor()
	}
	resultDay = nd
	resultDouble = math.RoundToEven(float64(span.Micros)*factor + secRemainder*usecsPerSecF)
	if math.IsNaN(resultDouble) || !fitsI64(resultDouble) {
		return Interval{}, oor()
	}
	return Interval{Months: resultMonth, Days: resultDay, Micros: int64(resultDouble)}, nil
}

// fractionToF64 converts an evaluated percentile fraction (the direct argument, evaluated per
// group) to f64 (aggregates.md §13/§17). nil / a NULL value → nil (a NULL fraction yields NULL). A
// numeric value (the resolver restricts the fraction to a numeric family) widens via the IEEE /
// correctly-rounded decimal cast. The range check (22003) is applied by the caller after this.
func fractionToF64(frac *Value) (*float64, error) {
	if frac == nil || frac.IsNull() {
		return nil, nil
	}
	switch frac.Kind {
	case ValFloat64:
		f := frac.F64()
		return &f, nil
	case ValFloat32:
		f := float64(frac.F32())
		return &f, nil
	case ValInt:
		f := float64(frac.Int)
		return &f, nil
	case ValDecimal:
		f, err := decimalToFloat64(*frac.decimal())
		if err != nil {
			return nil, err
		}
		return &f, nil
	default:
		panic("a non-numeric percentile fraction is rejected at resolve")
	}
}

// percentileDiscAt computes percentile_disc over the already-sorted group values: the value at row
// ceil(p·N) (1-based), i.e. the smallest K with K/N ≥ p (PG orderedsetaggs.c). Caller guarantees
// non-empty + the fraction in range. spec/design/aggregates.md §13.
func percentileDiscAt(vals []Value, p float64) Value {
	n := len(vals)
	// PG: rownum = ceil(p·N) (1-based), then the value at max(rownum, 1).
	rownum := int(math.Ceil(p * float64(n)))
	idx := 0
	if rownum >= 1 {
		idx = rownum - 1
	}
	if idx > n-1 {
		idx = n - 1
	}
	return vals[idx]
}

// percentileContAt computes percentile_cont over the already-sorted f64 group values: interpolate
// between the two bracketing rows, in f64 with PG's exact operation order — bit-identical across
// cores and to PG (spec/design/aggregates.md §13). Caller guarantees non-empty + the fraction in
// range.
func percentileContAt(floats []float64, p float64) float64 {
	n := len(floats)
	pos := p * float64(n-1)
	first := int(math.Floor(pos))
	second := int(math.Ceil(pos))
	if first == second {
		return floats[first]
	}
	lo, hi := floats[first], floats[second]
	proportion := pos - float64(first)
	return lo + (proportion * (hi - lo))
}

// dirCmp applies a WITHIN GROUP sort direction to a comparison result (DESC reverses).
func dirCmp(c int, desc bool) int {
	if desc {
		return -c
	}
	return c
}

// sortOsaVals sorts an ordered-set aggregate's buffered values by its WITHIN GROUP order
// (aggregates.md §13). With no collation, the value-canonical comparison (the same total order
// ORDER BY/MIN/MAX use). With a collation, a stable decorate-sort by the precomputed collation sort
// key bytes (a collated key is always text; an unmapped code point fails 0A000 at this deterministic
// point, like the query ORDER BY). The stable sort keeps collation-equal values in scan order, so
// the result is deterministic and cross-core identical.
func sortOsaVals(vals []Value, collation *Collation, desc bool) error {
	if collation == nil {
		sort.SliceStable(vals, func(i, j int) bool {
			return dirCmp(valueCmp(vals[i], vals[j]), desc) < 0
		})
		return nil
	}
	// Decorate each value with its collation sort key (text only), sort stably by the key bytes, then
	// undecorate. The keys are built once up front so a SortKey failure (0A000 for an unmapped code
	// point) surfaces at this deterministic point rather than inside the comparator.
	type deco struct {
		key []byte
		val Value
	}
	d := make([]deco, len(vals))
	for i, v := range vals {
		if v.Kind != ValText {
			panic("a collated WITHIN GROUP key buffers only text")
		}
		sk, err := sortKey(collation, v.str())
		if err != nil {
			return err
		}
		d[i] = deco{key: sk, val: v}
	}
	sort.SliceStable(d, func(i, j int) bool {
		return dirCmp(bytes.Compare(d[i].key, d[j].key), desc) < 0
	})
	for i := range d {
		vals[i] = d[i].val
	}
	return nil
}

// finalizeHypothetical computes a hypothetical-set aggregate's value (aggregates.md §19): given the
// buffered group key tuples rows, the per-group hypothetical row hyp, and the WITHIN GROUP per-key
// sort specs, count where hyp would rank. rank = 1 + rows strictly before hyp; dense_rank = 1 +
// distinct values strictly before; percent_rank = (rank-1)/N; cume_dist = (#rows ≤ hyp + 1)/(N+1) —
// PG's orderedsetaggs.c formulas exactly. Over an empty group: rank/dense_rank 1, percent_rank 0,
// cume_dist 1.
func finalizeHypothetical(plan aggPlan, rows [][]Value, hyp []Value, sorts []keySort) (Value, error) {
	n := len(rows)
	if n == 0 {
		switch plan {
		case planHypoRank, planHypoDenseRank:
			return IntValue(1), nil
		case planHypoPercentRank:
			return Float64Value(0.0), nil
		case planHypoCumeDist:
			return Float64Value(1.0), nil
		default:
			panic("finalizeHypothetical only for the hypothetical-set plans")
		}
	}
	var strictlyBefore int64
	var le int64 // rows that sort ≤ hyp (for cume_dist's rank with flag +1)
	// The distinct strictly-before key tuples (for dense_rank), value-canonical (the group-key
	// distinctRowKey, the same form the GROUP BY bucketing uses — collapses 1.5/1.50, NULL with NULL).
	distinct := make(map[string]bool)
	for _, r := range rows {
		ord, err := hypoCmp(r, hyp, sorts)
		if err != nil {
			return NullValue(), err
		}
		switch {
		case ord < 0:
			strictlyBefore++
			le++
			distinct[distinctRowKey(r)] = true
		case ord == 0:
			le++
		}
	}
	switch plan {
	case planHypoRank:
		return IntValue(strictlyBefore + 1), nil
	case planHypoDenseRank:
		return IntValue(int64(len(distinct)) + 1), nil
	case planHypoPercentRank:
		return Float64Value(float64(strictlyBefore) / float64(n)), nil
	case planHypoCumeDist:
		return Float64Value(float64(le+1) / float64(n+1)), nil
	default:
		panic("finalizeHypothetical only for the hypothetical-set plans")
	}
}

// hypoCmp compares a buffered key tuple a to the hypothetical row b by the WITHIN GROUP order
// (aggregates.md §19): the first key whose comparison is non-equal decides. Each key honors its NULL
// placement, direction, and collation (a collated text key can fail 0A000).
func hypoCmp(a, b []Value, sorts []keySort) (int, error) {
	for i, ks := range sorts {
		ord, err := compareHypoKey(a[i], b[i], ks)
		if err != nil {
			return 0, err
		}
		if ord != 0 {
			return ord, nil
		}
	}
	return 0, nil
}

// compareHypoKey compares one WITHIN GROUP key pair under its sort spec (NULL placement + direction +
// collation), mirroring the query ORDER BY key comparison plus the collated-text path (aggregates.md
// §19).
func compareHypoKey(a, b Value, ks keySort) (int, error) {
	switch {
	case a.IsNull() && b.IsNull():
		return 0, nil
	case a.IsNull():
		if ks.nullsFirst {
			return -1, nil
		}
		return 1, nil
	case b.IsNull():
		if ks.nullsFirst {
			return 1, nil
		}
		return -1, nil
	default:
		var base int
		if ks.collation != nil && a.Kind == ValText && b.Kind == ValText {
			c, err := collatedCmp(ks.collation, a.str(), b.str())
			if err != nil {
				return 0, err
			}
			base = c
		} else {
			base = valueCmp(a, b)
		}
		return dirCmp(base, ks.desc), nil
	}
}

// checkPercentileFraction is the percentile fraction range gate (aggregates.md §13): < 0, > 1, or
// NaN is 22003 (numeric_value_out_of_range), matching PG's "percentile value … is not between 0 and
// 1". Called per group at finalize, after the NULL-fraction check.
func checkPercentileFraction(p float64) error {
	if math.IsNaN(p) || p < 0 || p > 1 {
		return newError(NumericValueOutOfRange, fmt.Sprintf("percentile value %v is not between 0 and 1", p))
	}
	return nil
}

// percentileInputF64 widens a numeric value to f64 for percentile_cont (aggregates.md §13):
// integers via the IEEE cast, decimals via the correctly-rounded decimal→f64 cast (matching PG's
// numeric→float8), floats unchanged. The resolver restricts the operand to a numeric family.
func percentileInputF64(v Value) (float64, error) {
	switch v.Kind {
	case ValInt:
		return float64(v.Int), nil
	case ValFloat32, ValFloat64:
		return v.asF64(), nil
	case ValDecimal:
		return decimalToFloat64(*v.decimal())
	default:
		panic("resolver restricts percentile_cont to a numeric operand")
	}
}

// itemsHaveAggregate reports whether any select item contains an aggregate call.
func itemsHaveAggregate(items selectItems) bool {
	if items.All {
		return false
	}
	for _, it := range items.Items {
		if exprHasAggregate(it.Expr) {
			return true
		}
	}
	return false
}

// windowDefHasAggregate reports whether a window definition's PARTITION BY / ORDER BY keys contain
// an aggregate (`OVER (ORDER BY sum(x))` — spec/design/window.md §5.1). Such an aggregate makes the
// query an aggregate query (a whole-table aggregate if there is no GROUP BY), exactly as a top-level
// aggregate would, so the window keys resolve against the grouped row. Used by both the inline-over
// walk in exprHasAggregate and the WINDOW-clause scan that computes isAgg.
func windowDefHasAggregate(wd *windowDef) bool {
	for _, p := range wd.Partition {
		if exprHasAggregate(p) {
			return true
		}
	}
	for _, k := range wd.Order {
		if exprHasAggregate(k.Expr) {
			return true
		}
	}
	return false
}

// windowsHaveAggregate reports whether any WINDOW-clause entry's keys contain an aggregate (`WINDOW w
// AS (ORDER BY sum(x))`), which — like a top-level aggregate — makes the query an aggregate query
// (spec/design/window.md §5.1). The entries are still named references at this point (the OVER-name
// desugar runs later), so the WINDOW clause is scanned directly.
func windowsHaveAggregate(windows []namedWindow) bool {
	for i := range windows {
		if windowDefHasAggregate(&windows[i].Def) {
			return true
		}
	}
	return false
}

// isAggregateName reports whether name (case-insensitive) is a registered aggregate surface
// (COUNT/SUM/MIN/MAX/AVG). Data-driven over the catalog (Aggregates); consulted by the grouping
// + CHECK-structure walks.
func isAggregateName(name string) bool {
	lname := toLowerASCII(name)
	for i := range aggregates {
		if toLowerASCII(aggregates[i].Surface) == lname {
			return true
		}
	}
	if _, ok := objectAggClassify(name); ok {
		return true
	}
	// The ordered-set aggregates are aggregates for these purposes too (they fold a set of rows)
	// but are not catalog rows (their result/arg mold is special, like GROUPING()).
	return isOrderedSetAggregateName(name)
}

// isOrderedSetAggregateName reports whether name is an ordered-set aggregate surface (mode /
// percentile_cont / percentile_disc — spec/design/aggregates.md §13). These take a WITHIN GROUP
// (ORDER BY …) clause and are resolved by resolveOrderedSetAggregate, intercepted before the generic
// aggregate/scalar dispatch.
func isOrderedSetAggregateName(name string) bool {
	switch toLowerASCII(name) {
	case "mode", "percentile_cont", "percentile_disc":
		return true
	}
	return false
}

// isHypotheticalSetName reports whether name is a hypothetical-set aggregate surface — rank /
// dense_rank / percent_rank / cume_dist used with WITHIN GROUP (spec/design/aggregates.md §19).
// These names are ALSO window functions; the WITHIN GROUP clause routes them here instead of the
// window path.
func isHypotheticalSetName(name string) bool {
	switch toLowerASCII(name) {
	case "rank", "dense_rank", "percent_rank", "cume_dist":
		return true
	}
	return false
}

// objectAggClassify classifies a json[b]_object_agg[_unique] name → (plan, ok). These 2-argument
// aggregates are hand-resolved (the single-operand aggregate catalog can't express a key/value
// pair), like jsonb_set among the scalar functions (json-sql-functions.md §4).
func objectAggClassify(name string) (aggPlan, bool) {
	switch toLowerASCII(name) {
	case "jsonb_object_agg":
		return planJsonbObjectAgg, true
	case "json_object_agg":
		return planJsonObjectAgg, true
	case "jsonb_object_agg_unique":
		return planJsonbObjectAggUnique, true
	case "json_object_agg_unique":
		return planJsonObjectAggUnique, true
	default:
		return 0, false
	}
}

// isWindowOnlyName reports whether name is a registered WINDOW-only function surface
// (row_number/…). Data-driven over the catalog (Windows). Such a function REQUIRES an OVER clause —
// used without one it is 42809 (spec/design/window.md §7). The catalog aggregates double as window
// functions but are not in Windows, so they are still valid without OVER.
func isWindowOnlyName(name string) bool {
	lname := toLowerASCII(name)
	for i := range windows {
		if toLowerASCII(windows[i].Surface) == lname {
			return true
		}
	}
	return false
}

// subscriptSpecExprs returns the sub-expressions of one AST subscript spec (an index, or a slice's
// present bounds) — for the Expr tree walkers (spec/design/array.md §6).
func subscriptSpecExprs(s subscriptSpec) []*exprNode {
	if !s.IsSlice {
		return []*exprNode{s.Index}
	}
	var out []*exprNode
	if s.Lower != nil {
		out = append(out, s.Lower)
	}
	if s.Upper != nil {
		out = append(out, s.Upper)
	}
	return out
}

// exprHasAggregate reports whether an expression tree contains an AGGREGATE call anywhere. A
// scalar-function call is not itself an aggregate but may CONTAIN one (abs(sum(x))), so its
// arguments are walked.
func exprHasAggregate(e exprNode) bool {
	switch e.Kind {
	case exprFuncCall:
		// An aggregate name carrying OVER (inline or a named-window reference) is a WINDOW
		// function, not a bare aggregate (so a `sum(x) OVER ()` / `sum(x) OVER w` query is a window
		// query, not an aggregate query). Mirrors Rust: (over.is_none() && over_name.is_none() &&
		// is_aggregate_name(name)) || any arg has an aggregate. (Detection runs before the
		// OVER-name desugar.)
		if e.FuncCall.Over == nil && e.FuncCall.OverName == "" && isAggregateName(e.FuncCall.Name) {
			return true
		}
		// A hypothetical-set name with a WITHIN GROUP clause (`rank(x) WITHIN GROUP (…)`) is an
		// aggregate (aggregates.md §19), so the query is an aggregate query. Mirrors Rust's
		// (within_group.is_some() && is_hypothetical_set_name(name)).
		if e.FuncCall.WithinGroup != nil && isHypotheticalSetName(e.FuncCall.Name) {
			return true
		}
		for _, a := range e.FuncCall.Args {
			if exprHasAggregate(*a) {
				return true
			}
		}
		// An aggregate INSIDE the inline window definition's keys (`rank() OVER (ORDER BY sum(x))`)
		// also makes the query an aggregate query — those keys resolve against the grouped row (§5.1).
		if e.FuncCall.Over != nil && windowDefHasAggregate(e.FuncCall.Over) {
			return true
		}
		return false
	case exprCast:
		return exprHasAggregate(e.Cast.Inner)
	case exprExtract:
		return exprHasAggregate(e.Extract.Source)
	case exprCollate:
		return exprHasAggregate(e.Collate.Inner)
	case exprUnary:
		return exprHasAggregate(e.Unary.Operand)
	case exprIsNull:
		return exprHasAggregate(e.IsNullOf.Operand)
	case exprIsJson:
		return exprHasAggregate(e.IsJsonOf.Operand)
	case exprJsonCtor:
		return exprHasAggregate(e.JsonCtorOf.Operand)
	case exprJsonExists:
		return exprHasAggregate(e.JsonExists.Ctx) || exprHasAggregate(e.JsonExists.Path)
	case exprJsonValue:
		return exprHasAggregate(e.JsonValue.Ctx) || exprHasAggregate(e.JsonValue.Path)
	case exprJsonQuery:
		return exprHasAggregate(e.JsonQuery.Ctx) || exprHasAggregate(e.JsonQuery.Path)
	case exprBinary:
		return exprHasAggregate(e.Binary.Lhs) || exprHasAggregate(e.Binary.Rhs)
	case exprIsDistinct:
		return exprHasAggregate(e.IsDistinct.Lhs) || exprHasAggregate(e.IsDistinct.Rhs)
	case exprIn:
		if exprHasAggregate(e.In.Lhs) {
			return true
		}
		for _, elem := range e.In.List {
			if exprHasAggregate(elem) {
				return true
			}
		}
		return false
	case exprBetween:
		return exprHasAggregate(e.Between.Lhs) || exprHasAggregate(e.Between.Lo) || exprHasAggregate(e.Between.Hi)
	case exprLike:
		return exprHasAggregate(e.Like.Lhs) || exprHasAggregate(e.Like.Rhs)
	case exprRegex:
		return exprHasAggregate(e.Regex.Lhs) || exprHasAggregate(e.Regex.Rhs)
	case exprCase:
		if e.Case.Operand != nil && exprHasAggregate(*e.Case.Operand) {
			return true
		}
		for _, w := range e.Case.Whens {
			if exprHasAggregate(w.Cond) || exprHasAggregate(w.Result) {
				return true
			}
		}
		return e.Case.Els != nil && exprHasAggregate(*e.Case.Els)
	case exprFieldAccess, exprFieldStar:
		// Field selection `(expr).field` / `(expr).*` recurses into the composite base
		// (spec/design/composite.md §S4) — an aggregate hidden in the base must surface.
		return exprHasAggregate(*e.Base)
	case exprQualifiedStar:
		return false // `t.*` is a leaf relation reference — no aggregate
	case exprSubscript:
		// `base[..]` — an aggregate hidden in the base array or any subscript bound must surface.
		if exprHasAggregate(*e.Base) {
			return true
		}
		for _, s := range e.Subscripts {
			for _, x := range subscriptSpecExprs(s) {
				if exprHasAggregate(*x) {
					return true
				}
			}
		}
		return false
	case exprRow, exprArray:
		// A ROW(...) / ARRAY[...] constructor recurses into its element expressions.
		for _, it := range e.RowItems {
			if exprHasAggregate(it) {
				return true
			}
		}
		return false
	case exprQuantified:
		return exprHasAggregate(e.Quantified.Lhs) || exprHasAggregate(e.Quantified.Array)
	default:
		return false
	}
}

// itemsHaveWindow reports whether any select item contains a window-function call (a FuncCall
// carrying OVER). A window query resolves its projection in window mode (spec/design/window.md §5.1).
func itemsHaveWindow(items selectItems) bool {
	if items.All {
		return false
	}
	for _, it := range items.Items {
		if exprHasWindow(it.Expr) {
			return true
		}
	}
	return false
}

// orderByHasWindow reports whether any ORDER BY key is (or contains) a window function, so a query
// whose only OVER call sits in the ORDER BY still sets up the window machinery (grammar.md §10,
// window.md §5.1). An ordinal/column key carries no expression.
func orderByHasWindow(keys []orderKey) bool {
	for _, k := range keys {
		if k.Expr != nil && exprHasWindow(*k.Expr) {
			return true
		}
	}
	return false
}

// exprHasWindow reports whether an expression tree contains a window-function call anywhere (a
// FuncCall whose Over is set). An ordinary call may CONTAIN one in its arguments
// (abs(row_number() OVER ())), so the arguments are walked; a window call's own PARTITION BY /
// ORDER BY may not contain a window function (rejected at resolve, 42P20), so they are not walked
// here. A subquery is an independent query: a window function inside it is the subquery's own.
func exprHasWindow(e exprNode) bool {
	switch e.Kind {
	case exprFuncCall:
		if e.FuncCall.Over != nil || e.FuncCall.OverName != "" {
			return true
		}
		for _, a := range e.FuncCall.Args {
			if exprHasWindow(*a) {
				return true
			}
		}
		return false
	case exprCast:
		return exprHasWindow(e.Cast.Inner)
	case exprExtract:
		return exprHasWindow(e.Extract.Source)
	case exprCollate:
		return exprHasWindow(e.Collate.Inner)
	case exprUnary:
		return exprHasWindow(e.Unary.Operand)
	case exprIsNull:
		return exprHasWindow(e.IsNullOf.Operand)
	case exprIsJson:
		return exprHasWindow(e.IsJsonOf.Operand)
	case exprJsonCtor:
		return exprHasWindow(e.JsonCtorOf.Operand)
	case exprJsonExists:
		return exprHasWindow(e.JsonExists.Ctx) || exprHasWindow(e.JsonExists.Path)
	case exprJsonValue:
		return exprHasWindow(e.JsonValue.Ctx) || exprHasWindow(e.JsonValue.Path)
	case exprJsonQuery:
		return exprHasWindow(e.JsonQuery.Ctx) || exprHasWindow(e.JsonQuery.Path)
	case exprBinary:
		return exprHasWindow(e.Binary.Lhs) || exprHasWindow(e.Binary.Rhs)
	case exprIsDistinct:
		return exprHasWindow(e.IsDistinct.Lhs) || exprHasWindow(e.IsDistinct.Rhs)
	case exprIn:
		if exprHasWindow(e.In.Lhs) {
			return true
		}
		for _, elem := range e.In.List {
			if exprHasWindow(elem) {
				return true
			}
		}
		return false
	case exprBetween:
		return exprHasWindow(e.Between.Lhs) || exprHasWindow(e.Between.Lo) || exprHasWindow(e.Between.Hi)
	case exprLike:
		return exprHasWindow(e.Like.Lhs) || exprHasWindow(e.Like.Rhs)
	case exprRegex:
		return exprHasWindow(e.Regex.Lhs) || exprHasWindow(e.Regex.Rhs)
	case exprCase:
		if e.Case.Operand != nil && exprHasWindow(*e.Case.Operand) {
			return true
		}
		for _, w := range e.Case.Whens {
			if exprHasWindow(w.Cond) || exprHasWindow(w.Result) {
				return true
			}
		}
		return e.Case.Els != nil && exprHasWindow(*e.Case.Els)
	case exprFieldAccess, exprFieldStar:
		return exprHasWindow(*e.Base)
	case exprQualifiedStar:
		return false // `t.*` is a leaf relation reference — no window function

	case exprSubscript:
		if exprHasWindow(*e.Base) {
			return true
		}
		for _, s := range e.Subscripts {
			for _, x := range subscriptSpecExprs(s) {
				if exprHasWindow(*x) {
					return true
				}
			}
		}
		return false
	case exprRow, exprArray:
		for _, it := range e.RowItems {
			if exprHasWindow(it) {
				return true
			}
		}
		return false
	case exprQuantified:
		return exprHasWindow(e.Quantified.Lhs) || exprHasWindow(e.Quantified.Array)
	default:
		return false
	}
}

// extendWindow applies the base-window merge rules (spec/design/window.md §5, PostgreSQL
// transformWindowDefinitions): a definition that names a base copies the base's PARTITION BY and —
// if the base has one — its ORDER BY, and supplies its own frame. The extender may not add a
// PARTITION BY (42P20, even when the base has none), may add an ORDER BY only when the base has
// none (42P20 otherwise), and the base must not carry a frame (42P20). The three checks fire in
// PostgreSQL's priority order: PARTITION, then ORDER, then frame. Returns the merged inline
// definition (Base == "").
func extendWindow(base, ext windowDef, baseName string) (windowDef, error) {
	if len(ext.Partition) > 0 {
		return windowDef{}, newError(WindowingError, fmt.Sprintf("cannot override PARTITION BY clause of window %q", baseName))
	}
	if len(base.Order) > 0 && len(ext.Order) > 0 {
		return windowDef{}, newError(WindowingError, fmt.Sprintf("cannot override ORDER BY clause of window %q", baseName))
	}
	if base.Frame != nil {
		return windowDef{}, newError(WindowingError, fmt.Sprintf("cannot copy window %q because it has a frame clause", baseName))
	}
	order := ext.Order
	if len(base.Order) > 0 {
		order = base.Order
	}
	return windowDef{Base: "", Partition: base.Partition, Order: order, Frame: ext.Frame}, nil
}

// resolveWindowClause resolves a WINDOW clause into all-inline definitions (spec/design/window.md
// §5). Entries are processed left-to-right; an entry naming a base extends an already-resolved
// earlier entry (a self- or forward-reference is therefore "does not exist" — 42704), via
// extendWindow. Every entry is resolved — even ones no OVER references — matching PostgreSQL's
// whole-clause check.
func resolveWindowClause(windows []namedWindow) ([]namedWindow, error) {
	resolved := make([]namedWindow, 0, len(windows))
	for _, nw := range windows {
		r := nw.Def
		if nw.Def.Base != "" {
			base, err := lookupWindow(resolved, nw.Def.Base)
			if err != nil {
				return nil, err
			}
			r, err = extendWindow(base, nw.Def, nw.Def.Base)
			if err != nil {
				return nil, err
			}
		}
		resolved = append(resolved, namedWindow{Name: nw.Name, Def: r})
	}
	return resolved, nil
}

// lookupWindow finds a (resolved, Base == "") window definition by name in windows,
// case-insensitively, or raises 42704 `window "<name>" does not exist`.
func lookupWindow(windows []namedWindow, name string) (windowDef, error) {
	for i := range windows {
		if strings.EqualFold(windows[i].Name, name) {
			return windows[i].Def, nil
		}
	}
	return windowDef{}, newError(UndefinedObject, fmt.Sprintf("window %q does not exist", name))
}

// desugarItems desugars `OVER name` / `OVER (base …)` references in a select list to their
// WINDOW-clause definitions before resolution (spec/design/window.md §5): a pure OverName reference
// gets the named definition copied into Over, an inline Over with a Base is merged onto the named
// base (extendWindow); an undefined name is 42704. After this every window call carries an inline
// Over (Base == ""), so resolution (S0–S4) handles named and inline windows uniformly. Returns a
// fresh SelectItems (the original AST is not mutated — the FuncCall pointers along each rewritten
// path are freshly allocated).
func desugarItems(items selectItems, windows []namedWindow) (selectItems, error) {
	if items.All {
		return items, nil
	}
	out := selectItems{Items: make([]selectItem, len(items.Items))}
	for i, it := range items.Items {
		e, err := desugarNamedWindows(it.Expr, windows)
		if err != nil {
			return selectItems{}, err
		}
		out.Items[i] = selectItem{Expr: e, Alias: it.Alias}
	}
	return out, nil
}

// desugarNamedWindows recursively rewrites every `OVER name` (OverName set) in e to its definition
// from windows (copied into Over), erroring 42704 if the name is absent. Mirrors Rust's
// desugar_named_windows: the FuncCall arm rewrites the reference and recurses into the arguments;
// the other arms recurse into their sub-expressions; leaves, subscripts, and subqueries (independent)
// carry no top-level window ref to rewrite. The walk returns a fresh Expr so the original AST stays
// unmutated.
func desugarNamedWindows(e exprNode, windows []namedWindow) (exprNode, error) {
	switch e.Kind {
	case exprFuncCall:
		fc := *e.FuncCall // shallow copy; we replace Args/Over/OverName below
		if fc.OverName != "" {
			// `OVER name` — a pure reference: copy the named definition whole, frame included (no
			// merge rules; copying a framed window is forbidden only for the extend form below — §5).
			def, err := lookupWindow(windows, fc.OverName)
			if err != nil {
				return exprNode{}, err
			}
			fc.Over = &def
			fc.OverName = ""
		} else if fc.Over != nil && fc.Over.Base != "" {
			// `OVER (base …)` — an extend: merge the inline definition onto the named base.
			base, err := lookupWindow(windows, fc.Over.Base)
			if err != nil {
				return exprNode{}, err
			}
			merged, err := extendWindow(base, *fc.Over, fc.Over.Base)
			if err != nil {
				return exprNode{}, err
			}
			fc.Over = &merged
		}
		if len(fc.Args) > 0 {
			args := make([]*exprNode, len(fc.Args))
			for i, a := range fc.Args {
				na, err := desugarNamedWindows(*a, windows)
				if err != nil {
					return exprNode{}, err
				}
				args[i] = &na
			}
			fc.Args = args
		}
		ne := e
		ne.FuncCall = &fc
		return ne, nil
	case exprCast:
		inner, err := desugarNamedWindows(e.Cast.Inner, windows)
		if err != nil {
			return exprNode{}, err
		}
		nc := *e.Cast
		nc.Inner = inner
		ne := e
		ne.Cast = &nc
		return ne, nil
	case exprExtract:
		src, err := desugarNamedWindows(e.Extract.Source, windows)
		if err != nil {
			return exprNode{}, err
		}
		nx := *e.Extract
		nx.Source = src
		ne := e
		ne.Extract = &nx
		return ne, nil
	case exprCollate:
		inner, err := desugarNamedWindows(e.Collate.Inner, windows)
		if err != nil {
			return exprNode{}, err
		}
		nc := *e.Collate
		nc.Inner = inner
		ne := e
		ne.Collate = &nc
		return ne, nil
	case exprUnary:
		op, err := desugarNamedWindows(e.Unary.Operand, windows)
		if err != nil {
			return exprNode{}, err
		}
		nu := *e.Unary
		nu.Operand = op
		ne := e
		ne.Unary = &nu
		return ne, nil
	case exprIsNull:
		op, err := desugarNamedWindows(e.IsNullOf.Operand, windows)
		if err != nil {
			return exprNode{}, err
		}
		ni := *e.IsNullOf
		ni.Operand = op
		ne := e
		ne.IsNullOf = &ni
		return ne, nil
	case exprIsJson:
		op, err := desugarNamedWindows(e.IsJsonOf.Operand, windows)
		if err != nil {
			return exprNode{}, err
		}
		ni := *e.IsJsonOf
		ni.Operand = op
		ne := e
		ne.IsJsonOf = &ni
		return ne, nil
	case exprJsonCtor:
		op, err := desugarNamedWindows(e.JsonCtorOf.Operand, windows)
		if err != nil {
			return exprNode{}, err
		}
		ni := *e.JsonCtorOf
		ni.Operand = op
		ne := e
		ne.JsonCtorOf = &ni
		return ne, nil
	case exprJsonExists:
		ctx, err := desugarNamedWindows(e.JsonExists.Ctx, windows)
		if err != nil {
			return exprNode{}, err
		}
		path, err := desugarNamedWindows(e.JsonExists.Path, windows)
		if err != nil {
			return exprNode{}, err
		}
		nj := *e.JsonExists
		nj.Ctx = ctx
		nj.Path = path
		ne := e
		ne.JsonExists = &nj
		return ne, nil
	case exprJsonValue:
		ctx, err := desugarNamedWindows(e.JsonValue.Ctx, windows)
		if err != nil {
			return exprNode{}, err
		}
		path, err := desugarNamedWindows(e.JsonValue.Path, windows)
		if err != nil {
			return exprNode{}, err
		}
		nj := *e.JsonValue
		nj.Ctx = ctx
		nj.Path = path
		ne := e
		ne.JsonValue = &nj
		return ne, nil
	case exprJsonQuery:
		ctx, err := desugarNamedWindows(e.JsonQuery.Ctx, windows)
		if err != nil {
			return exprNode{}, err
		}
		path, err := desugarNamedWindows(e.JsonQuery.Path, windows)
		if err != nil {
			return exprNode{}, err
		}
		nj := *e.JsonQuery
		nj.Ctx = ctx
		nj.Path = path
		ne := e
		ne.JsonQuery = &nj
		return ne, nil
	case exprBinary:
		lhs, err := desugarNamedWindows(e.Binary.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		rhs, err := desugarNamedWindows(e.Binary.Rhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		nb := *e.Binary
		nb.Lhs = lhs
		nb.Rhs = rhs
		ne := e
		ne.Binary = &nb
		return ne, nil
	case exprIsDistinct:
		lhs, err := desugarNamedWindows(e.IsDistinct.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		rhs, err := desugarNamedWindows(e.IsDistinct.Rhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		nd := *e.IsDistinct
		nd.Lhs = lhs
		nd.Rhs = rhs
		ne := e
		ne.IsDistinct = &nd
		return ne, nil
	case exprIn:
		lhs, err := desugarNamedWindows(e.In.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		list := make([]exprNode, len(e.In.List))
		for i, x := range e.In.List {
			nx, err := desugarNamedWindows(x, windows)
			if err != nil {
				return exprNode{}, err
			}
			list[i] = nx
		}
		nin := *e.In
		nin.Lhs = lhs
		nin.List = list
		ne := e
		ne.In = &nin
		return ne, nil
	case exprQuantified:
		lhs, err := desugarNamedWindows(e.Quantified.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		arr, err := desugarNamedWindows(e.Quantified.Array, windows)
		if err != nil {
			return exprNode{}, err
		}
		nq := *e.Quantified
		nq.Lhs = lhs
		nq.Array = arr
		ne := e
		ne.Quantified = &nq
		return ne, nil
	case exprBetween:
		lhs, err := desugarNamedWindows(e.Between.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		lo, err := desugarNamedWindows(e.Between.Lo, windows)
		if err != nil {
			return exprNode{}, err
		}
		hi, err := desugarNamedWindows(e.Between.Hi, windows)
		if err != nil {
			return exprNode{}, err
		}
		nbt := *e.Between
		nbt.Lhs = lhs
		nbt.Lo = lo
		nbt.Hi = hi
		ne := e
		ne.Between = &nbt
		return ne, nil
	case exprLike:
		lhs, err := desugarNamedWindows(e.Like.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		rhs, err := desugarNamedWindows(e.Like.Rhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		nl := *e.Like
		nl.Lhs = lhs
		nl.Rhs = rhs
		ne := e
		ne.Like = &nl
		return ne, nil
	case exprRegex:
		lhs, err := desugarNamedWindows(e.Regex.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		rhs, err := desugarNamedWindows(e.Regex.Rhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		nr := *e.Regex
		nr.Lhs = lhs
		nr.Rhs = rhs
		ne := e
		ne.Regex = &nr
		return ne, nil
	case exprRow, exprArray:
		items := make([]exprNode, len(e.RowItems))
		for i, x := range e.RowItems {
			nx, err := desugarNamedWindows(x, windows)
			if err != nil {
				return exprNode{}, err
			}
			items[i] = nx
		}
		ne := e
		ne.RowItems = items
		return ne, nil
	case exprQualifiedStar:
		return e, nil // a leaf relation reference — no named window to desugar
	case exprFieldAccess, exprFieldStar:
		base, err := desugarNamedWindows(*e.Base, windows)
		if err != nil {
			return exprNode{}, err
		}
		ne := e
		ne.Base = &base
		return ne, nil
	case exprCase:
		nc := *e.Case
		if e.Case.Operand != nil {
			op, err := desugarNamedWindows(*e.Case.Operand, windows)
			if err != nil {
				return exprNode{}, err
			}
			nc.Operand = &op
		}
		whens := make([]caseWhen, len(e.Case.Whens))
		for i, w := range e.Case.Whens {
			cond, err := desugarNamedWindows(w.Cond, windows)
			if err != nil {
				return exprNode{}, err
			}
			res, err := desugarNamedWindows(w.Result, windows)
			if err != nil {
				return exprNode{}, err
			}
			whens[i] = caseWhen{Cond: cond, Result: res}
		}
		nc.Whens = whens
		if e.Case.Els != nil {
			els, err := desugarNamedWindows(*e.Case.Els, windows)
			if err != nil {
				return exprNode{}, err
			}
			nc.Els = &els
		}
		ne := e
		ne.Case = &nc
		return ne, nil
	default:
		// Leaves, subscripts, and subqueries (independent) carry no top-level window ref to rewrite.
		return e, nil
	}
}
