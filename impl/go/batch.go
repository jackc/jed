package jed

// Vectorized batch execution (Go-core-internal; CLAUDE.md §2 "execution strategy is a per-core
// free choice"). The PAX + vectorization program's executor track: a single-base-table aggregate
// with no blocking operator beyond the fold runs a columnar fast path instead of the row-at-a-time
// group machinery (executor.go execSelectEmit's isAgg branch). Value stays the fallback currency —
// any shape this file does not specialize takes the existing eager path, byte-identical by
// construction.
//
//   - Stage 1 — WHOLE-TABLE integer/float aggregate (no GROUP BY): folds a decoded column in a tight
//     loop (foldAggBatch) instead of building one transient Value per row + dispatching through the
//     group map.
//   - Stage 2 — SINGLE INTEGER-KEY GROUP BY: buckets survivors by the raw int64 key into a
//     map[int64]int (a non-NULL integer's value-canonical distinctRowKey is a bijection on its
//     int64, so an int64 map yields the SAME buckets as the scalar map[string]int keyed on
//     distinctRowKey — and NULLs bucket together via a sentinel group), then folds each group's
//     accumulators through the shared acc.fold (byte-identical acc state, hence finalize).
//     Float SUM/AVG kernels round out the numeric folds (MIN/MAX were already type-agnostic).
//   - A2 — COLUMNAR GATHER (packed-leaf.md §11 Track A2): a FILTER-FREE file-backed aggregate (either
//     stage above) gathers ONLY its touched columns into dense per-column lanes straight off the
//     Packed leaves (aggColumnar → ColumnarScanMasked → foldAggColumnar / groupByIntKeyColumnar),
//     NEVER materializing a full-width storedRow. This is the allocation dividend the row feed leaves
//     on the table: a wide-table single-column scan drops from O(rows × columns) to O(rows) allocation
//     (a 64-column count(*) went from ~100 MB of all-NULL rows to a few KB). Cost-neutral by
//     construction — it charges the same page_read / value_decompress / storage_row_read block the row
//     feed does. Gated to file-backed stores (an in-memory store's row path already shares its rows
//     zero-copy) and declines to the row path on any filter / spillable column, so the fold kernels
//     stay the same code either way (foldAggColumnar mirrors foldAggBatch, reading lane[i] instead of
//     survivors[i][idx]).
//   - A2 PROJECTION FEED (packed-leaf.md §11 Track A2): the same columnar gather for a bare-column
//     PROJECTION scan (`SELECT c0, c3 FROM t [WHERE pk = …]`, no ORDER BY/LIMIT/aggregate) —
//     projectColumnar gathers the projected columns into lanes and returns an emitColumnar emitter that
//     builds each output row directly from the lanes, so the materialize path's full-width storedRow per
//     record is never allocated (the projection's B/op dividend, the sibling of the aggregate one). Same
//     file-backed / non-spillable gate; the emitColumnar drive charges row_produced per emitted row
//     exactly like the emitProject drive over a bare-column projection (a zero-cost slot read).
//   - A3 — FILTER VECTORIZATION (packed-leaf.md §11 Track A3): a WHERE predicate no longer forces the
//     row path. filterColumnar evaluates plan.filter over the gathered lanes (via a single reusable
//     scratch row) into a selection vector of survivor indices, so a FILTERED aggregate or projection
//     also gathers columnar — the fold / emit then visits only the selected lane positions (never a
//     full-width row). It reuses the scalar rExpr.eval verbatim, so the filter's operator_eval charges,
//     its 3VL survivor test, AND its result stay byte-identical to the scalar filter loop — the row path
//     already feeds the filter a masked row (untouched columns NULL), and the filter references only
//     masked columns (collectTouched includes plan.filter), so a scratch row filled from the lanes is
//     identical input. Same file-backed / non-spillable gate (a filter over a spillable column keeps the
//     row path — the lanes carry no unfetched values).
//
// The load-bearing invariant is that BOTH the result multiset AND the deterministic cost
// (CLAUDE.md §8/§13) stay byte-identical to the scalar path; the conformance corpus asserts
// `# cost:` on every core, so a cost divergence fails loudly. Three properties keep it identical:
//   - The scan + WHERE cost is reused verbatim — the scan runs through the same materializeRel
//     (its page_read / value_decompress / storage_row_read block) and the WHERE predicate through
//     the same rExpr.eval (its operator_eval charges + 3VL survivor test). Only the group/fold
//     machinery is replaced.
//   - The fold charges aggregate_accumulate once per (survivor × aggregate) exactly like the scalar
//     loop, in bulk. The bucketing + finalize are UNMETERED in the scalar path too (cost.md §3), so
//     an int64 map vs a distinctRowKey-string map is a pure internal choice with zero cost impact.
//   - Group emission order is scan-order-of-first-appearance — IDENTICAL to the scalar insertion
//     order (both bucket the same post-WHERE rows in scan order) — so even a LIMIT without ORDER BY
//     keeps the same rows, not merely the same multiset.
//
// The path is gated to the UNMETERED lane (costMeter.unmetered — no ceiling / lifetime budget /
// cancellation armed), which is the conformance/bench case: with nothing to abort, Guard is a
// no-op, so a bulk aggregate_accumulate charge reproduces the scalar total with no per-row Guard.
// A metered query keeps the scalar path, so its deterministic abort row is unchanged.

// colBatch is a decoded integer column pulled from a set of rows for a vectorized fold. Its int lane
// is filled by striding the inline scalar of each Value (Value.Int, i16/i32/i64 all live there) plus
// a per-row NULL flag. extractIntColumn is the single seam PAX (Stage 3) later turns into a
// near-memcpy from a column-major leaf page — the fold kernels below are unchanged by that.
type colBatch struct {
	n     int
	ints  []int64 // Value.Int strided; meaningful only where !nulls[i]
	nulls []bool  // true == the row's value is NULL (skipped by SUM/MIN/MAX, uncounted by COUNT(expr))
}

// extractIntColumn strides column idx of each row into a colBatch (the int + null lanes). Over the
// current row-major layout this touches each already-decoded Value once; the dense []int64 it
// produces is what lets the fold loops below run without per-cell Value dispatch (and what the Go
// compiler can auto-vectorize). Every value of an integer column is ValInt or ValNull — the
// eligibility gate guarantees an integer operand, so no other Kind reaches here.
func extractIntColumn(rows []storedRow, idx int) colBatch {
	n := len(rows)
	b := colBatch{n: n, ints: make([]int64, n), nulls: make([]bool, n)}
	for i := range rows {
		v := rows[i][idx]
		if v.Kind == ValNull {
			b.nulls[i] = true
		} else {
			b.ints[i] = v.Int
		}
	}
	return b
}

// vectorizedAggEligible reports whether plan is a shape execVectorizedAgg specializes: a
// single-base-table SUM/COUNT/MIN/MAX/AVG with no DISTINCT / FILTER / HAVING / window / ORDER BY,
// over a full or primary-key-bounded scan, that is EITHER whole-table (no GROUP BY) OR grouped by a
// single bare integer column. Mostly pure plan inspection — it charges nothing, so a bail is free
// and the general path runs with identical results + cost; the single-key case additionally reads
// the group-key column's static type from the table store (a one-time lookup, not per row) to
// confirm it is a scalar integer (so the int64-keyed bucket is a bijection of distinctRowKey).
func (db *engine) vectorizedAggEligible(plan *selectPlan) bool {
	if !plan.isAgg {
		return false
	}
	// One base table, no join.
	if len(plan.rels) != 1 || len(plan.joins) != 0 {
		return false
	}
	rel := &plan.rels[0]
	if rel.srf != nil || rel.cte != nil || rel.derived != nil || rel.lateral {
		return false
	}
	// Full scan or a primary-key bound only — an index / GIN / GiST / point-set (OR/IN) bound changes
	// the scan mechanics and residual filter, so it keeps the scalar / eager path (cost.md §3).
	if len(plan.relBounds) > 0 && plan.relBounds[0].needsEagerScan() {
		return false
	}
	// Exactly one grouping set (ROLLUP/CUBE/GROUPING SETS produce several — deferred), no materialized
	// expression keys (`GROUP BY a + b`), and no GROUPING() calls.
	if len(plan.groupSets) != 1 || len(plan.groupExprs) != 0 || len(plan.groupingSpecs) != 0 {
		return false
	}
	gset := &plan.groupSets[0]
	switch len(gset.keyCols) {
	case 0:
		// Whole-table aggregation (Stage 1): the () grand-total group, no master grouping columns.
		if len(plan.groupKeys) != 0 {
			return false
		}
	case 1:
		// Single-key GROUP BY (Stage 2): the sole master grouping column is this key, its synthetic
		// slot is 0, and the key is a bare scalar-INTEGER column of the base table (so the int64
		// bucket key is a bijection of the scalar path's distinctRowKey — see file header).
		if len(plan.groupKeys) != 1 || plan.groupKeys[0] != gset.keyCols[0] {
			return false
		}
		if len(gset.slotSrc) != 1 || gset.slotSrc[0] != 0 {
			return false
		}
		store := db.lkpStore(rel.tableName)
		if store == nil {
			return false
		}
		ord := gset.keyCols[0] - rel.offset
		if ord < 0 || ord >= len(store.colTypes) {
			return false
		}
		ct := store.colTypes[ord]
		if ct.Composite || ct.Elem != nil || ct.RangeElem != nil || !ct.Scalar.IsInteger() {
			return false
		}
	default:
		return false
	}
	// No blocking / re-shaping operator beyond the fold (each would add cost the fast path would have
	// to mirror; deferred). LIMIT/OFFSET is honored below via windowBounds, so it need not bail.
	if plan.distinct || plan.having != nil || plan.hasWindow || len(plan.order) != 0 {
		return false
	}
	if len(plan.aggSpecs) == 0 {
		return false
	}
	for i := range plan.aggSpecs {
		if !vectorizedSpecEligible(&plan.aggSpecs[i]) {
			return false
		}
	}
	return true
}

// vectorizedSpecEligible reports whether one aggregate is a specialized numeric kernel: a plain
// (non-DISTINCT, non-FILTER, non-ordered-set, non-hypothetical) COUNT(*) / COUNT(col) / SUM(i16|i32)
// / SUM|AVG(f32|f64) / MIN(col) / MAX(col) whose operand (where it has one) is a bare column
// reference. SUM(i64|decimal) → planSumDecimal and AVG(decimal) → planAvg are deferred (their fold
// charges running-sum-dependent decimal_work); MIN/MAX fold ANY type through valueCmp.
func vectorizedSpecEligible(spec *aggSpec) bool {
	if spec.distinct || spec.filter != nil || spec.osaFrac != nil || spec.hypo != nil {
		return false
	}
	switch spec.plan {
	case planCountStar:
		return spec.operand == nil
	case planCount, planSumInt, planSumFloat32, planSumFloat64,
		planAvgFloat32, planAvgFloat64, planMin, planMax:
		return spec.operand != nil && spec.operand.kind == reColumn
	default:
		return false
	}
}

// execVectorizedAgg runs a vectorizedAggEligible plan and returns a fully-formed, already-projected
// result (emitter{mode: emitFinal}): one row for a whole-table grand total, or one per group for a
// single-key GROUP BY. It reuses the scalar scan + WHERE for exact cost + survivor determination,
// folds each aggregate over the survivors with a columnar kernel (whole-table) or per-group through
// acc.fold (grouped), then produces the output rows exactly as the emitProject drive would (Guard,
// row_produced, projection eval) under the query's LIMIT/OFFSET window. Only runs on the unmetered
// lane (the caller gates).
func (db *engine) execVectorizedAgg(plan *selectPlan, outer []storedRow, params []Value, ctes cteCtx, rng *stmtRng, meter *costMeter) (emitter, error) {
	env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}
	gset := &plan.groupSets[0]

	// A2/A3 columnar fast path (packed-leaf.md §11 Track A2/A3): a file-backed aggregate gathers only its
	// touched columns into dense lanes and folds columnar — never a full-width storedRow, the allocation
	// dividend A1 leaves on the table. A WHERE predicate (A3) is applied over the lanes into a selection
	// vector rather than forcing the row path. Declines (ok=false) to the row path below for an in-memory
	// store or a spillable touched column. Cost-neutral by construction (aggColumnar charges the identical
	// scan + filter block).
	{
		srows, ok, err := db.aggColumnar(plan, gset, env, meter)
		if err != nil {
			return emitter{}, err
		}
		if ok {
			return db.emitAggSyntheticRows(plan, srows, env, meter)
		}
	}

	// Row path: scan the single base relation through the same path the eager executor uses, so the
	// page_read / value_decompress / storage_row_read block is charged identically (executor.go
	// materializeRel).
	rows, err := db.materializeRel(plan, 0, params, outer, nil, rng, ctes, meter)
	if err != nil {
		return emitter{}, err
	}

	// WHERE: evaluate the residual predicate per scanned row through the ordinary evaluator, so its
	// operator_eval charges and its 3VL survivor test (keep iff TRUE — the same IsTrue as the scalar
	// WHERE loop) are byte-identical. Filter in place: survivors is a prefix of rows.
	survivors := rows
	if plan.filter != nil {
		survivors = rows[:0]
		for _, r := range rows {
			v, ferr := plan.filter.eval(r, env, meter)
			if ferr != nil {
				return emitter{}, ferr
			}
			if v.IsTrue() {
				survivors = append(survivors, r)
			}
		}
	}

	// Build the synthetic grouped rows [key?, agg results…] in scan-order-of-first-appearance, then
	// finalize each group's accumulators. Whole-table (no key) folds columnar; a single-key GROUP BY
	// buckets by int64 and folds per group. Either way finalize + the projection below read the accs
	// positionally, exactly like the scalar synthetic row.
	var srows []storedRow
	if len(gset.keyCols) == 0 {
		accs := newAccsForSpecs(plan.aggSpecs)
		for i := range plan.aggSpecs {
			if ferr := foldAggBatch(accs[i], &plan.aggSpecs[i], survivors, meter); ferr != nil {
				return emitter{}, ferr
			}
		}
		srow, ferr := finalizeGroup(nil, accs)
		if ferr != nil {
			return emitter{}, ferr
		}
		srows = []storedRow{srow}
	} else {
		srows, err = db.groupByIntKey(plan, gset, survivors, meter)
		if err != nil {
			return emitter{}, err
		}
	}
	return db.emitAggSyntheticRows(plan, srows, env, meter)
}

// emitAggSyntheticRows emits the synthetic grouped rows (a whole-table grand total or one per group)
// under the query's LIMIT/OFFSET window, exactly as the emitProject drive does: for each windowed row,
// Guard, row_produced, then the projection list (charging its operator_evals). Only emitted rows are
// projected + charged (the §3 asymmetry). Returned as emitFinal — already projected + charged — so
// neither the eager nor the lazy drive charges again; aggWindowBounds mirrors the execSelectEmit
// windowBounds closure (clamped in the i64 domain). Shared verbatim by the row (foldAggBatch) and
// columnar (aggColumnar) fold paths, so emission + cost are identical either way.
func (db *engine) emitAggSyntheticRows(plan *selectPlan, srows []storedRow, env *evalEnv, meter *costMeter) (emitter, error) {
	start, end := aggWindowBounds(plan, int64(len(srows)))
	out := make([][]Value, 0, end-start)
	for _, srow := range srows[start:end] {
		if gerr := meter.Guard(); gerr != nil {
			return emitter{}, gerr
		}
		meter.Charge(costs.RowProduced)
		projected := make([]Value, len(plan.projections))
		for i, p := range plan.projections {
			v, perr := p.eval(srow, env, meter)
			if perr != nil {
				return emitter{}, perr
			}
			projected[i] = v
		}
		out = append(out, projected)
	}
	return emitter{final: out, mode: emitFinal}, nil
}

// aggColumnar runs the A2/A3 columnar gather for a vectorized aggregate (packed-leaf.md §11 Track
// A2/A3): it scans only the touched columns of the single base relation into dense per-column lanes
// (never a full-width storedRow), charges the identical scan cost block, applies any WHERE predicate
// over the lanes into a selection vector (A3), and folds each aggregate columnar over the survivors,
// returning the finalized synthetic rows — the whole-table grand total or one row per group.
// Returns ok=false (declining to the caller's row path) when the store is in-memory (its Decoded leaves
// share their rows zero-copy on the row path, so a lane gather would only add allocation with no
// packed-leaf win to offset it), when a touched column can spill (the columnar feed has no
// value-resolution step — this also covers a filter over a spillable column), or when a needed column
// index is out of range / unmasked (a safety net, never expected for an eligible plan — e.g. a non-zero
// relation offset). Cost-neutral by construction: same page_read (same node visits), value_decompress
// (0 — no spillable touched column), storage_row_read (× rowCount), operator_eval (the filter over each
// scanned row), aggregate_accumulate (× survivors), and row_produced as the row path.
func (db *engine) aggColumnar(plan *selectPlan, gset *groupSetPlan, env *evalEnv, meter *costMeter) ([]storedRow, bool, error) {
	rel := &plan.rels[0]
	store := db.lkpStore(rel.tableName)
	if store == nil {
		return nil, false, nil
	}
	// File-backed only (see the doc comment): an in-memory store's row path is already zero-copy.
	if store.paging == nil {
		return nil, false, nil
	}
	mask := plan.relMasks[0]
	// No touched column may spill — so the feed's value_decompress slab count is 0 and no unfetched
	// value is left unresolved. An eligible aggregate touches only integer operands + an integer key
	// (plus the filter's columns, which the scratch row reads); this guard declines a filter or operand
	// over a spillable (text/decimal/…) column to the row path, keeping the lanes free of unfetched
	// values.
	if anySpillableMasked(store.colTypes, mask) {
		return nil, false, nil
	}
	// Every column the fold reads (each aggregate operand + the group key) must be a valid, masked
	// table ordinal — else its gathered lane would be nil. For a single base table these are 0-based
	// table ordinals in [0, K); the check also declines a (never-expected) non-zero relation offset.
	need := func(idx int) bool { return idx >= 0 && idx < len(mask) && mask[idx] }
	for i := range plan.aggSpecs {
		if op := plan.aggSpecs[i].operand; op != nil && !need(op.index) {
			return nil, false, nil
		}
	}
	if len(gset.keyCols) == 1 && !need(gset.keyCols[0]) {
		return nil, false, nil
	}

	// Determine the scan bound exactly as materializeRel does: a PK-range bound, or the full scan. An
	// empty bound (a contradictory PK predicate) admits no rows — skip the scan entirely (0 pages/rows),
	// matching materializeRel's `if !empty` guard.
	cols := make([][]Value, len(mask))
	rowCount, pages, slabs := 0, 0, 0
	scan := true
	b := unboundedBound()
	if sb := plan.relBounds[0]; sb != nil && sb.pk != nil {
		var empty bool
		if b, empty = db.buildKeyBound(sb.pk, env.params, env.outer, nil); empty {
			scan = false
		}
	}
	if scan {
		var err error
		if cols, rowCount, pages, slabs, err = store.ColumnarScanMasked(b, mask); err != nil {
			return nil, false, err
		}
	}
	// Charge the scan cost block identically to materializeRel + scanSource: page_read × nodes,
	// value_decompress × slabs (0 here), storage_row_read × rowCount. On the unmetered lane (the caller
	// gates) this bulk charge reproduces the scanSource's per-row accrual (Guard is a no-op).
	meter.Charge(costs.PageRead*int64(pages) + costs.ValueDecompress*int64(slabs) + costs.StorageRowRead*int64(rowCount))

	// A3: apply the WHERE predicate over the lanes into a selection vector (nil ⇒ all rows survive). The
	// filter's operator_eval charges + 3VL survivor test are byte-identical to the scalar filter loop
	// (filterColumnar reuses plan.filter.eval), and nsurv is the survivor count the fold accumulates.
	var sel []int32
	nsurv := rowCount
	if plan.filter != nil {
		var err error
		if sel, err = filterColumnar(plan.filter, cols, mask, rowCount, env, meter); err != nil {
			return nil, false, err
		}
		nsurv = len(sel)
	}

	if len(gset.keyCols) == 0 {
		accs := newAccsForSpecs(plan.aggSpecs)
		for i := range plan.aggSpecs {
			spec := &plan.aggSpecs[i]
			var lane []Value
			if spec.operand != nil {
				lane = cols[spec.operand.index]
			}
			if err := foldAggColumnar(accs[i], spec, lane, sel, nsurv, meter); err != nil {
				return nil, false, err
			}
		}
		srow, err := finalizeGroup(nil, accs)
		if err != nil {
			return nil, false, err
		}
		return []storedRow{srow}, true, nil
	}
	srows, err := groupByIntKeyColumnar(plan, gset, cols, sel, nsurv, meter)
	if err != nil {
		return nil, false, err
	}
	return srows, true, nil
}

// filterColumnar evaluates plan.filter over the gathered per-column lanes and returns the surviving row
// indices (the selection vector) — filter vectorization (packed-leaf.md §11 Track A3). It reuses the
// scalar rExpr.eval verbatim over a SINGLE reusable scratch row (the masked columns filled from the
// lanes at that row index, untouched columns left NULL), so the predicate's operator_eval charges and
// its 3VL survivor test (keep iff TRUE) are byte-identical to the scalar WHERE loop — and the result is
// identical too, because the row path also feeds the filter a MASKED row (untouched columns NULL via
// resolveColumns / rowAtMasked) and the filter references only masked columns (collectTouched includes
// plan.filter), so a scratch row filled from the lanes is the same input. The one reusable scratch row
// is the allocation win: no full-width storedRow per scanned row, only the int32 survivor indices. The
// caller has verified no touched column spills, so every masked lane is a non-nil []Value of length
// rowCount (an untouched column's lane stays nil but is never read — the filter references only touched
// columns).
func filterColumnar(filter *rExpr, cols [][]Value, mask []bool, rowCount int, env *evalEnv, meter *costMeter) ([]int32, error) {
	sel := make([]int32, 0, rowCount)
	scratch := make(storedRow, len(mask))
	for i := 0; i < rowCount; i++ {
		for c := range mask {
			if mask[c] {
				scratch[c] = cols[c][i]
			}
		}
		v, err := filter.eval(scratch, env, meter)
		if err != nil {
			return nil, err
		}
		if v.IsTrue() {
			sel = append(sel, int32(i))
		}
	}
	return sel, nil
}

// vectorizedProjectEligible reports whether plan is a shape projectColumnar specializes: a bare-column
// projection over a single base table with no join / aggregate / window / DISTINCT / ORDER BY / LIMIT /
// OFFSET and no index/GIN/GiST bound — i.e. a plain `SELECT c0, c3, … FROM t [WHERE …]` whose output is
// the (optionally filtered) scan-order rows narrowed to a column subset. A residual filter is allowed
// (A3): projectColumnar applies it over the lanes into a selection vector. Pure plan inspection (charges
// nothing), so a bail is free and the general materialize path runs with identical results + cost; the
// store / paging / spillable / column-range gates live in projectColumnar, which declines to that path.
// LIMIT/OFFSET is excluded deliberately: a LIMIT with no ORDER BY streams with an early exit
// (streamingScanEligible), which projectColumnar's whole-table gather must not steal.
func (db *engine) vectorizedProjectEligible(plan *selectPlan) bool {
	if plan.isAgg || plan.hasWindow || plan.distinct {
		return false
	}
	// One base table, no join.
	if len(plan.rels) != 1 || len(plan.joins) != 0 {
		return false
	}
	rel := &plan.rels[0]
	if rel.srf != nil || rel.cte != nil || rel.derived != nil || rel.lateral {
		return false
	}
	// No ORDER BY / LIMIT / OFFSET (those route to a streaming / sort / index path). A residual filter is
	// fine — projectColumnar vectorizes it (A3).
	if len(plan.order) != 0 || plan.limit != nil || plan.offset != nil {
		return false
	}
	// Full scan or a primary-key bound only — an index / GIN / GiST / point-set (OR/IN) bound changes
	// the scan mechanics and residual filter, so it keeps the eager path (cost.md §3).
	if len(plan.relBounds) > 0 && plan.relBounds[0].needsEagerScan() {
		return false
	}
	// Every projection must be a bare column reference: a bare reColumn evaluates to row[index] with zero
	// operator_eval, so gathering it from a dense lane is cost-identical. An expression projection
	// (`c0 + 1`, a function call) charges operator_eval and needs a row — it keeps the row path.
	if len(plan.projections) == 0 {
		return false
	}
	for _, p := range plan.projections {
		if p.kind != reColumn {
			return false
		}
	}
	return true
}

// projectColumnar runs the A2/A3 columnar gather for a vectorizedProjectEligible plan (packed-leaf.md
// §11 Track A2/A3): it scans only the touched columns of the single base relation into dense per-column
// lanes (never a full-width storedRow), charges the identical scan cost block, applies any WHERE
// predicate over the lanes into a selection vector (A3), and returns an emitColumnar emitter that gathers
// each surviving output row from the lanes on emission. Returns ok=false (declining to the caller's row
// path) for an in-memory store (its Decoded leaves share rows zero-copy, so a lane gather would only add
// allocation with no packed-leaf win to offset it), a spillable touched column (the columnar feed has no
// value-resolution step — this also covers a filter over a spillable column), or a projection column that
// is out of range / unmasked (a safety net, never expected — a projected column is touched, hence
// masked). Cost-neutral by construction: same page_read (same node visits) / value_decompress (0 — no
// spillable touched column) / storage_row_read (× rowCount) / operator_eval (the filter over each scanned
// row) as the row path, then row_produced per emitted (surviving) row charged by the emitColumnar drive —
// exactly the emitProject drive over a bare-column projection.
func (db *engine) projectColumnar(plan *selectPlan, env *evalEnv, meter *costMeter) (emitter, bool, error) {
	rel := &plan.rels[0]
	store := db.lkpStore(rel.tableName)
	if store == nil {
		return emitter{}, false, nil
	}
	// File-backed only (see aggColumnar): an in-memory store's row path is already zero-copy.
	if store.paging == nil {
		return emitter{}, false, nil
	}
	mask := plan.relMasks[0]
	// No touched column may spill — so the feed's value_decompress slab count is 0 and no unfetched value
	// is left unresolved (the columnar feed has no resolveColumns step). The mask includes the filter's
	// columns (collectTouched), so this also declines a filter over a spillable column to the row path.
	if anySpillableMasked(store.colTypes, mask) {
		return emitter{}, false, nil
	}
	// Each projected column must be a valid, masked table ordinal — else its gathered lane would be nil.
	// For a single base table a projection's reColumn index is a 0-based table ordinal in [0, K); a
	// projected column is always touched (hence masked), so this holds — the check also declines a
	// (never-expected) synthetic slot or non-zero relation offset.
	projCols := make([]int, len(plan.projections))
	for i, p := range plan.projections {
		idx := p.index
		if idx < 0 || idx >= len(mask) || !mask[idx] {
			return emitter{}, false, nil
		}
		projCols[i] = idx
	}

	// Determine the scan bound exactly as materializeRel does: a PK-range bound, or the full scan. An
	// empty bound (a contradictory PK predicate) admits no rows — skip the scan entirely (0 pages/rows).
	cols := make([][]Value, len(mask))
	rowCount, pages, slabs := 0, 0, 0
	scan := true
	b := unboundedBound()
	if len(plan.relBounds) > 0 {
		if sb := plan.relBounds[0]; sb != nil && sb.pk != nil {
			var empty bool
			if b, empty = db.buildKeyBound(sb.pk, env.params, env.outer, nil); empty {
				scan = false
			}
		}
	}
	if scan {
		var err error
		if cols, rowCount, pages, slabs, err = store.ColumnarScanMasked(b, mask); err != nil {
			return emitter{}, false, err
		}
	}
	// Charge the scan cost block identically to materializeRel + scanSource: page_read × nodes,
	// value_decompress × slabs (0 here), storage_row_read × rowCount. On the unmetered lane (the caller
	// gates) this bulk charge reproduces the scanSource's per-row accrual (Guard is a no-op).
	meter.Charge(costs.PageRead*int64(pages) + costs.ValueDecompress*int64(slabs) + costs.StorageRowRead*int64(rowCount))

	// A3: apply the WHERE predicate over the lanes into a selection vector (nil ⇒ all rows survive). The
	// emitColumnar drive emits len(sel) rows, mapping output row j to lane position sel[j].
	var sel []int32
	nEmit := int64(rowCount)
	if plan.filter != nil {
		s, err := filterColumnar(plan.filter, cols, mask, rowCount, env, meter)
		if err != nil {
			return emitter{}, false, err
		}
		sel = s
		nEmit = int64(len(sel))
	}

	return emitter{cols: cols, projCols: projCols, sel: sel, start: 0, end: nEmit, mode: emitColumnar}, true, nil
}

// aggWindowBounds computes the LIMIT/OFFSET [start,end) window over n synthetic rows, mirroring the
// windowBounds closure in execSelectEmit — clamped in the i64 domain against the row count before
// indexing so a huge LIMIT/OFFSET never truncates or panics (CLAUDE.md §8; grammar.md §9). A
// whole-table aggregate with LIMIT 0 / OFFSET 1 therefore emits zero rows, like the scalar path.
func aggWindowBounds(plan *selectPlan, n int64) (int64, int64) {
	start := int64(0)
	if plan.offset != nil && *plan.offset < n {
		start = *plan.offset
	} else if plan.offset != nil {
		start = n
	}
	end := n
	if plan.limit != nil && *plan.limit < n-start {
		end = start + *plan.limit
	}
	return start, end
}

// newAccsForSpecs builds one accumulator per aggregate spec (whole-table or one group), via the same
// newAccFromSpec the scalar group machinery uses — so acc state + finalize are identical.
func newAccsForSpecs(specs []aggSpec) []*acc {
	accs := make([]*acc, len(specs))
	for i := range specs {
		accs[i] = newAccFromSpec(specs[i])
	}
	return accs
}

// finalizeGroup builds one synthetic output row [key…, agg results…] from a group's key values (nil
// for the whole-table grand total) and its finalized accumulators. Eligible specs are never
// ordered-set / hypothetical / GROUPING, so acc.finalize alone yields the result (no osaFrac / hypo
// / mask handling — those shapes are gated out).
func finalizeGroup(keys []Value, accs []*acc) (storedRow, error) {
	srow := make([]Value, 0, len(keys)+len(accs))
	srow = append(srow, keys...)
	for _, a := range accs {
		v, ferr := a.finalize()
		if ferr != nil {
			return nil, ferr
		}
		srow = append(srow, v)
	}
	return srow, nil
}

// groupByIntKey buckets the survivor rows by their single integer group-key column and folds each
// aggregate per group, returning the finalized synthetic rows [key, agg results…] in
// scan-order-of-first-appearance. The bucket is a map[int64]int over the raw key (a bijection of the
// scalar distinctRowKey for a fixed-width integer column) plus one sentinel group for NULL keys
// (distinctRowKey groups all NULLs together). The fold reuses acc.fold (byte-identical acc state);
// aggregate_accumulate is charged once per (survivor × spec) in bulk — the identical total to the
// scalar loop, which charges it unconditionally per row for every non-FILTER spec. The bucketing
// itself is unmetered (cost.md §3), so the int64 map is a free internal choice.
func (db *engine) groupByIntKey(plan *selectPlan, gset *groupSetPlan, survivors []storedRow, meter *costMeter) ([]storedRow, error) {
	keyIdx := gset.keyCols[0]
	type vgroup struct {
		key  Value
		accs []*acc
	}
	var groups []vgroup
	index := make(map[int64]int)
	nullGI := -1

	meter.Charge(costs.AggregateAccumulate * int64(len(survivors)) * int64(len(plan.aggSpecs)))
	for _, r := range survivors {
		kv := r[keyIdx]
		var gi int
		if kv.Kind == ValNull {
			if nullGI < 0 {
				nullGI = len(groups)
				groups = append(groups, vgroup{key: kv, accs: newAccsForSpecs(plan.aggSpecs)})
			}
			gi = nullGI
		} else {
			var ok bool
			if gi, ok = index[kv.Int]; !ok {
				gi = len(groups)
				index[kv.Int] = gi
				groups = append(groups, vgroup{key: kv, accs: newAccsForSpecs(plan.aggSpecs)})
			}
		}
		// Fold each aggregate into this row's group. acc.fold charges nothing for the eligible plans
		// (COUNT/SUM-int/float/MIN/MAX) — the aggregate_accumulate was charged in bulk above — and a
		// bare-column operand's value is r[operand.index] (== operand.eval, which charges 0). COUNT(*)
		// has no operand; fold ignores the value.
		accs := groups[gi].accs
		for i := range plan.aggSpecs {
			v := NullValue()
			if op := plan.aggSpecs[i].operand; op != nil {
				v = r[op.index]
			}
			if ferr := accs[i].fold(v, meter); ferr != nil {
				return nil, ferr
			}
		}
	}

	out := make([]storedRow, 0, len(groups))
	for i := range groups {
		srow, ferr := finalizeGroup([]Value{groups[i].key}, groups[i].accs)
		if ferr != nil {
			return nil, ferr
		}
		out = append(out, srow)
	}
	return out, nil
}

// groupByIntKeyColumnar is the columnar twin of groupByIntKey (packed-leaf.md §11 Track A2/A3): it
// buckets the SURVIVOR rows by their single integer group-key column and folds each aggregate per group,
// reading the pre-gathered dense lanes (cols[keyCols[0]] for the key, cols[operand.index] for each
// operand) instead of striding a []storedRow — no full-width row. sel is the A3 selection vector (nil ⇒
// every scanned row survives); nsurv is the survivor count (len(sel), or rowCount when sel is nil). The
// buckets (a map[int64]int over the raw key plus one sentinel NULL group), the scan-order-of-first-
// appearance emission, the bulk aggregate_accumulate charge (× nsurv × specs), and acc.fold are
// byte-identical to groupByIntKey, so the acc state + finalize + cost match. The caller (aggColumnar) has
// verified every needed lane is populated (need()), so cols[keyCols[0]] and each cols[operand.index] are
// non-nil of length rowCount.
func groupByIntKeyColumnar(plan *selectPlan, gset *groupSetPlan, cols [][]Value, sel []int32, nsurv int, meter *costMeter) ([]storedRow, error) {
	keyLane := cols[gset.keyCols[0]]
	type vgroup struct {
		key  Value
		accs []*acc
	}
	var groups []vgroup
	index := make(map[int64]int)
	nullGI := -1

	meter.Charge(costs.AggregateAccumulate * int64(nsurv) * int64(len(plan.aggSpecs)))
	for j := 0; j < nsurv; j++ {
		i := j
		if sel != nil {
			i = int(sel[j])
		}
		kv := keyLane[i]
		var gi int
		if kv.Kind == ValNull {
			if nullGI < 0 {
				nullGI = len(groups)
				groups = append(groups, vgroup{key: kv, accs: newAccsForSpecs(plan.aggSpecs)})
			}
			gi = nullGI
		} else {
			var ok bool
			if gi, ok = index[kv.Int]; !ok {
				gi = len(groups)
				index[kv.Int] = gi
				groups = append(groups, vgroup{key: kv, accs: newAccsForSpecs(plan.aggSpecs)})
			}
		}
		accs := groups[gi].accs
		for s := range plan.aggSpecs {
			v := NullValue()
			if op := plan.aggSpecs[s].operand; op != nil {
				v = cols[op.index][i]
			}
			if ferr := accs[s].fold(v, meter); ferr != nil {
				return nil, ferr
			}
		}
	}

	out := make([]storedRow, 0, len(groups))
	for i := range groups {
		srow, ferr := finalizeGroup([]Value{groups[i].key}, groups[i].accs)
		if ferr != nil {
			return nil, ferr
		}
		out = append(out, srow)
	}
	return out, nil
}

// foldAggColumnar is the columnar twin of foldAggBatch (packed-leaf.md §11 Track A2/A3): it folds one
// WHOLE-TABLE aggregate over a pre-gathered dense column lane (nil for COUNT(*), whose count needs no
// values), reading the lane directly with no full-width storedRow. sel is the A3 selection vector (nil ⇒
// every scanned row survives); nsurv is the survivor count (len(sel), or the lane length when sel is
// nil). It reconstructs exactly the acc state foldAggBatch would leave so acc.finalize is unchanged, and
// charges aggregate_accumulate once per SURVIVOR (× nsurv), bulk on the unmetered lane. The
// int/float/min/max kernels are identical to foldAggBatch's, reading lane[sel[j]] (or lane[i] when
// unfiltered) instead of survivors[i][idx]. laneAt centralizes the sel indirection so the kernels read
// survivors in scan order either way (the overflow trap lands at the same running point).
func foldAggColumnar(a *acc, spec *aggSpec, lane []Value, sel []int32, nsurv int, meter *costMeter) error {
	meter.Charge(costs.AggregateAccumulate * int64(nsurv))
	laneAt := func(j int) Value {
		if sel != nil {
			return lane[sel[j]]
		}
		return lane[j]
	}
	switch spec.plan {
	case planCountStar:
		a.count += int64(nsurv)
	case planCount:
		for j := 0; j < nsurv; j++ {
			if laneAt(j).Kind != ValNull {
				a.count++
			}
		}
	case planSumInt:
		for j := 0; j < nsurv; j++ {
			v := laneAt(j)
			if v.Kind == ValNull {
				continue
			}
			// The identical i64 overflow trap (22003) as foldAggBatch / the scalar SUM fold, at the same
			// running point in scan order.
			s := a.sumInt + v.Int
			if (v.Int > 0 && s < a.sumInt) || (v.Int < 0 && s > a.sumInt) {
				return overflowErr(scalarInt64)
			}
			a.sumInt = s
			a.seen = true
		}
	case planSumFloat32, planSumFloat64, planAvgFloat32, planAvgFloat64:
		for j := 0; j < nsurv; j++ {
			if v := laneAt(j); v.Kind != ValNull {
				a.floatSum.add(v)
			}
		}
	case planMin, planMax:
		for j := 0; j < nsurv; j++ {
			v := laneAt(j)
			if v.Kind == ValNull {
				continue
			}
			if !a.hasCur {
				a.cur, a.hasCur = v, true
				continue
			}
			c := valueCmp(a.cur, v)
			keepCur := (spec.plan == planMin && c <= 0) || (spec.plan == planMax && c >= 0)
			if !keepCur {
				a.cur = v
			}
		}
	default:
		panic("foldAggColumnar: ineligible aggregate plan")
	}
	return nil
}

// foldAggBatch folds one WHOLE-TABLE aggregate over the survivor rows into acc, reconstructing
// exactly the state the scalar fold (executor.go acc.fold) would leave so acc.finalize is unchanged.
// It charges aggregate_accumulate once per survivor (the scalar loop charges it per (row × spec)),
// bulk on the unmetered lane. COUNT/SUM-int stride the int lane via a colBatch; MIN/MAX and the
// float SUM/AVG folds run over the raw Values (valueCmp / floatSum.add — type-agnostic, matching the
// scalar arm exactly).
func foldAggBatch(a *acc, spec *aggSpec, survivors []storedRow, meter *costMeter) error {
	meter.Charge(costs.AggregateAccumulate * int64(len(survivors)))
	switch spec.plan {
	case planCountStar:
		a.count += int64(len(survivors))
	case planCount:
		b := extractIntColumn(survivors, spec.operand.index)
		for i := 0; i < b.n; i++ {
			if !b.nulls[i] {
				a.count++
			}
		}
	case planSumInt:
		b := extractIntColumn(survivors, spec.operand.index)
		for i := 0; i < b.n; i++ {
			if b.nulls[i] {
				continue
			}
			// The identical i64 overflow trap (22003) as the scalar SUM fold, at the same running
			// point in scan order — so an overflow raises byte-identically (SUM over i16/i32 into i64
			// is effectively unreachable in practice, but the trap is preserved for correctness).
			s := a.sumInt + b.ints[i]
			if (b.ints[i] > 0 && s < a.sumInt) || (b.ints[i] < 0 && s > a.sumInt) {
				return overflowErr(scalarInt64)
			}
			a.sumInt = s
			a.seen = true
		}
	case planSumFloat32, planSumFloat64, planAvgFloat32, planAvgFloat64:
		// Float SUM/AVG collect the non-NULL inputs into the canonical-order fold accumulator; the
		// actual fold happens at finalize (order-independent — float.md §7). Byte-identical to the
		// scalar fold's floatSum.add(v) — the whole-table win is skipping the per-row Value rebuild +
		// eval dispatch, not the O(1) add.
		idx := spec.operand.index
		for i := range survivors {
			v := survivors[i][idx]
			if v.Kind != ValNull {
				a.floatSum.add(v)
			}
		}
	case planMin, planMax:
		idx := spec.operand.index
		for i := range survivors {
			v := survivors[i][idx]
			if v.Kind == ValNull {
				continue
			}
			if !a.hasCur {
				a.cur, a.hasCur = v, true
				continue
			}
			c := valueCmp(a.cur, v)
			keepCur := (spec.plan == planMin && c <= 0) || (spec.plan == planMax && c >= 0)
			if !keepCur {
				a.cur = v
			}
		}
	default:
		panic("foldAggBatch: ineligible aggregate plan")
	}
	return nil
}
