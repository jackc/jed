package jed

import "slices"

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
//   - A2 — FOLD DURING THE WALK (packed-leaf.md §11 Track A2): a file-backed aggregate (either stage
//     above) folds each scanned row's touched columns — read on demand via colAt straight off the
//     Packed leaves — into its accumulator during ONE tree walk (aggColumnar → pMap.foldScan), NEVER
//     materializing a full-width storedRow OR a per-column lane. Because SUM/COUNT/MIN/MAX/AVG are all
//     single-pass running accumulators (int/decimal exact; float a streaming scan-order total —
//     float.md §7), the aggregate needs O(1) state, so the whole-column gather the lane path once did
//     was pure waste: peak memory drops from O(rows) to O(1) (a full-table count+sum went from ~170 MB
//     of gathered Values to ~0). Cost-neutral by construction — foldScan visits the same nodes and rows
//     as the lane scan, so the page_read / value_decompress (0) / storage_row_read / aggregate_accumulate
//     block is charged identically. Gated to file-backed stores (an in-memory store's row path already
//     shares its rows zero-copy).
//   - A2 PROJECTION FEED (packed-leaf.md §11 Track A2): the same columnar gather for a bare-column
//     PROJECTION scan (`SELECT c0, c3 FROM t [WHERE pk = …]`, no ORDER BY/LIMIT/aggregate) —
//     projectColumnar gathers the projected columns into lanes and returns an emitColumnar emitter that
//     builds each output row directly from the lanes, so the materialize path's full-width storedRow per
//     record is never allocated (the projection's B/op dividend, the sibling of the aggregate one). The
//     emitColumnar drive charges row_produced per emitted row exactly like the emitProject drive over a
//     bare-column projection (a zero-cost slot read).
//   - A3 — FILTER VECTORIZATION (packed-leaf.md §11 Track A3): a WHERE predicate no longer forces the
//     row path. Both lanes evaluate it inline during foldScan, per record, and fold / gather only the
//     survivors.
//   - A4 — VARIABLE-WIDTH LANES (packed-leaf.md §11 Track A4, span_filter.go): a touched variable-width
//     column no longer declines the lanes. The WHERE compiles into span atoms evaluated on the stored
//     bytes (IS [NOT] NULL, text/bytea =/<>, array @>/<@/&&) plus generic predicates the scalar
//     rExpr.eval runs verbatim over a scratch row of resolved values, so the filter's operator_eval
//     charges, its 3VL survivor test, AND its result stay byte-identical to the scalar filter loop.
//     COUNT(col) reads only nullness; each admitted record's large-value units are read off its stored
//     tags.
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
// Bulk charges preserve the scalar total. Cancellation is polled at batch boundaries (at most 256
// scan/fold records). Under a cost ceiling or lifetime budget the aggregate lane runs as an attempt
// that the row path replaces unless it finishes cleanly under every ceiling (cost.md §6.1,
// execSelectEmit), so a metered query's deterministic abort is still the scalar path's; the
// projection lane still requires no ceiling (costMeter.canBatchCosts).
//
// A5 — THE LANES WIDENED (packed-leaf.md §11 Track A5; memory.md §5.4, §6.6.1): SUM/AVG over an
// integer or decimal fold on the lane through the shared acc.fold (so their running-sum
// decimal_work is the row path's); a grouped fold on a spill-capable host keeps a bounded group
// table that hands over to the spill structures (the end of this file) and leaves as a spool, which an
// ORDER BY then sorts through the bounded-spill lane's own stage.

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
	if len(plan.phys.relBounds) > 0 && plan.phys.relBounds[0].needsEagerScan() {
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
		store := db.lkpStoreScoped(rel.db, rel.tableName)
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
	// to mirror; deferred). LIMIT/OFFSET is honored below via windowBounds, so it need not bail. An
	// ORDER BY is the dispatch gate's to admit (aggregateLaneEligible): only the bounded grouped lane
	// sorts its groups.
	if plan.distinct || plan.distinctOn > 0 || plan.having != nil || plan.hasWindow {
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
// (non-DISTINCT, non-FILTER, non-ordered-set, non-hypothetical) COUNT(*) / COUNT(col) / SUM / AVG /
// MIN(col) / MAX(col) whose operand (where it has one) is a bare column reference. The decimal folds
// — SUM(i64|decimal) → planSumDecimal and AVG over an integer or decimal → planAvg — charge a
// running-sum-dependent decimal_work, but inside the shared acc.fold, and each accumulator meets its
// group's survivors in scan order on either path, so every fold charges the same amount; MIN/MAX
// fold ANY type through valueCmp.
func vectorizedSpecEligible(spec *aggSpec) bool {
	if spec.distinct || spec.filter != nil || spec.osaFrac != nil || spec.hypo != nil || spec.order != nil {
		return false
	}
	switch spec.plan {
	case planCountStar:
		return spec.operand == nil
	case planCount, planSumInt, planSumDecimal, planAvg, planSumFloat32, planSumFloat64,
		planAvgFloat32, planAvgFloat64, planMin, planMax:
		// A bare column, or an expression operand (memory.md §5.4): evaluated per survivor, and safe
		// to discard so a run that raises can be replaced by the row path (cost.md §6.1).
		return spec.operand != nil && (spec.operand.kind == reColumn || discardSafe(spec.operand))
	default:
		return false
	}
}

// execVectorizedAgg runs an aggregateLaneEligible plan and returns its synthetic group rows as an
// emitProject buffer (emitAggSyntheticRows): one row for a whole-table grand total, or one per group
// for a single-key GROUP BY. It reuses the scalar scan + WHERE for exact cost + survivor
// determination, folds each aggregate over the survivors with a columnar kernel (whole-table) or
// per-group through acc.fold (grouped); the emitter drive then produces the output rows (Guard,
// row_produced, projection eval) under the query's LIMIT/OFFSET window, exactly as for a scalar
// aggregate result. A grouped fold on a spill-capable host instead returns its groups as a spool
// (memory.md §6.6.1), sorted by the bounded-spill lane's ORDER BY stage and emitted as that lane
// emits its own.
func (db *engine) execVectorizedAgg(plan *selectPlan, outer []storedRow, params []Value, ctes cteCtx, rng *stmtRng, meter *costMeter) (emitter, error) {
	env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}
	gset := &plan.groupSets[0]

	// A2/A3 columnar fast path (packed-leaf.md §11 Track A2/A3): a page-backed aggregate folds only its
	// touched columns during one walk — never a full-width storedRow. A WHERE predicate is evaluated
	// per record during the walk. Declines (ok=false) to the row path below for an unpacked store.
	// Cost-neutral by construction (aggColumnar charges the identical scan + filter block).
	{
		own := &blockingOwner{db: db}
		var escaped *rowSpool
		defer func() { own.closeAll(escaped) }()
		srows, spool, ok, err := db.aggColumnar(plan, gset, env, meter, own)
		if err != nil {
			return emitter{}, err
		}
		if ok && spool != nil {
			// A grouped fold on a spill-capable host: its groups are a spool, handed over or not —
			// sorted by the bounded-spill lane's own ORDER BY stage and emitted under the LIMIT/OFFSET
			// window exactly as that lane emits its own.
			if len(plan.order) > 0 {
				if spool, err = db.blockingOrder(plan, spool, env, meter, own); err != nil {
					return emitter{}, meter.costFirst(err)
				}
			}
			total := spool.count
			start := int64(0)
			if plan.offset != nil {
				start = min(*plan.offset, total)
			}
			count := total - start
			if plan.limit != nil {
				count = min(count, *plan.limit)
			}
			sorted, err := spool.output()
			if err != nil {
				return emitter{}, err
			}
			escaped = spool
			for i := int64(0); i < start; i++ {
				if _, _, err := sorted.next(); err != nil {
					sorted.close()
					return emitter{}, err
				}
			}
			return emitter{sorted: sorted, mode: emitSorted, end: count}, nil
		}
		if ok {
			// The fold's group table is operator state; the group rows are a row buffer (memory.md §5.4).
			if err := admitAggRows(srows, meter); err != nil {
				return emitter{}, err
			}
			return orderAggSyntheticRows(plan, srows, env, meter)
		}
		// The lane declined a grouped fold on a spill-capable host: the bounded-spill lane, never the
		// unbounded row path below.
		if len(gset.keyCols) != 0 && db.boundedBlockingEligible(plan) {
			return db.execBoundedBlocking(plan, env, meter)
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
	// The same row-buffer accounting as the scalar aggregate branch (memory.md §5).
	mask := plan.relMasks[0]
	survivors := rows
	if plan.filter != nil {
		survivors = rows[:0]
		for _, r := range rows {
			v, ferr := plan.filter.evalFilter(r, env, meter)
			if ferr != nil {
				return emitter{}, ferr
			}
			if v {
				survivors = append(survivors, r)
			} else {
				meter.releaseRowMasked(r, mask)
			}
		}
	}

	// Build the synthetic grouped rows [key?, agg results…] in scan-order-of-first-appearance, then
	// finalize each group's accumulators. Whole-table (no key) folds columnar; a single-key GROUP BY
	// buckets by int64 and folds per group. Either way finalize + the projection below read the accs
	// positionally, exactly like the scalar synthetic row.
	var srows []storedRow
	if len(gset.keyCols) == 0 {
		accs := newAccsForSpecs(plan.aggSpecs, db.session.timeZone)
		// The whole-table group's entry (memory.md §6.2), returned once the group is finalized.
		charge := meter.stateCharge()
		defer charge.releaseAll() // includes cancellation during the fallback fold
		if charge.active() {
			if err := charge.reserve(meter, memEntryBytes(nil)); err != nil {
				return emitter{}, err
			}
		}
		for i := range plan.aggSpecs {
			if ferr := foldAggBatch(accs[i], &plan.aggSpecs[i], survivors, env, meter); ferr != nil {
				return emitter{}, ferr
			}
		}
		srow, ferr := finalizeGroup(nil, accs)
		charge.releaseAll()
		if ferr != nil {
			return emitter{}, ferr
		}
		srows = []storedRow{srow}
	} else {
		srows, err = db.groupByIntKey(plan, gset, survivors, env, meter)
		if err != nil {
			return emitter{}, err
		}
	}
	if err := admitAggRows(srows, meter); err != nil {
		return emitter{}, err
	}
	releaseRowsMasked(meter, survivors, mask)
	return orderAggSyntheticRows(plan, srows, env, meter)
}

// admitAggRows admits the finalized synthetic group rows of a vectorized aggregate as a row buffer
// (memory.md §5.1).
func admitAggRows(srows []storedRow, meter *costMeter) error {
	if !meter.queryMemoryActive() {
		return nil
	}
	for _, r := range srows {
		if err := meter.admitRow(r); err != nil {
			return err
		}
	}
	return nil
}

// emitAggSyntheticRows emits the synthetic grouped rows (a whole-table grand total or one per group)
// under the query's LIMIT/OFFSET window as an emitProject buffer: the drive charges Guard, row_produced,
// then the projection list per windowed row (the §3 asymmetry — only emitted rows are projected +
// charged), exactly as for a scalar aggregate result, and the eager drive admits each output row and
// releases the group rows (memory.md §5). aggWindowBounds mirrors the execSelectEmit windowBounds
// closure (clamped in the i64 domain). Shared verbatim by the row (foldAggBatch) and columnar
// (aggColumnar) fold paths, so emission + cost are identical either way.
// orderAggSyntheticRows is emitAggSyntheticRows after an ORDER BY over the groups of the unbounded
// form: the eager aggregate branch's own stage — each order-expression value appended to its group
// row and admitted, then the in-place sort (memory.md §5.4).
func orderAggSyntheticRows(plan *selectPlan, srows []storedRow, env *evalEnv, meter *costMeter) (emitter, error) {
	if len(plan.order) > 0 {
		if err := materializeOrderExprs(srows, plan.orderExprs, env, meter); err != nil {
			return emitter{}, err
		}
		if err := meter.costFirst(sortRows(srows, plan.order, meter.query, meter)); err != nil {
			return emitter{}, err
		}
	}
	return emitAggSyntheticRows(plan, srows), nil
}

func emitAggSyntheticRows(plan *selectPlan, srows []storedRow) emitter {
	start, end := aggWindowBounds(plan, int64(len(srows)))
	return emitter{src: srows, start: start, end: end, mode: emitProject}
}

// aggColumnar runs the columnar aggregate lane (packed-leaf.md §11 Tracks A2–A4): it walks the single
// base relation's bound once, evaluates any WHERE per record as a span filter (laneFilter — atoms on the
// stored bytes, generic predicates over a reusable scratch row), and folds each survivor's touched
// columns straight into its accumulator — never a full-width storedRow, and COUNT(col) reads only
// nullness — returning the finalized synthetic rows: the whole-table grand total or one row per group.
// Returns ok=false (declining to the caller's row path) when the store is in-memory (its Decoded leaves
// share their rows zero-copy on the row path, so a lane gather would only add allocation), or when a
// needed column index is out of range / unmasked (a safety net, never expected for an eligible plan —
// e.g. a non-zero relation offset). Cost-neutral by construction: same page_read (same node visits,
// plus each admitted record's chain pages), value_decompress, storage_row_read (× rowCount),
// operator_eval (the filter over each scanned row), aggregate_accumulate (× survivors), and
// row_produced as the row path.
//
// On a spill-capable host a grouped fold is bounded (memory.md §6.6.1): its group table is measured
// and charged as a resident keyed state map would be, hands over to the spill structures (owned by
// own) past work_mem or when the account turns a reservation away, and its groups come back as a
// spool (the second result) instead of rows.
func (db *engine) aggColumnar(plan *selectPlan, gset *groupSetPlan, env *evalEnv, meter *costMeter, own *blockingOwner) ([]storedRow, *rowSpool, bool, error) {
	rel := &plan.rels[0]
	store := db.lkpStoreScoped(rel.db, rel.tableName)
	if store == nil {
		return nil, nil, false, nil
	}
	// Packed-page stores include committed memory databases. Unpacked working stores decline.
	if store.paging == nil {
		return nil, nil, false, nil
	}
	mask := plan.relMasks[0]
	// A touched variable-width column adds each admitted record's large-value units (chain pages,
	// decompress slabs) to the scan block, read off its stored tag (Track A4).
	spillable := anySpillableMasked(store.colTypes, mask)
	unitCols := spillUnitCols(store.colTypes, mask)
	// Every column the fold reads (each aggregate operand + the group key) must be a valid, masked
	// table ordinal — else its gathered lane would be nil. For a single base table these are 0-based
	// table ordinals in [0, K); the check also declines a (never-expected) non-zero relation offset.
	need := func(idx int) bool { return idx >= 0 && idx < len(mask) && mask[idx] }
	for i := range plan.aggSpecs {
		if op := plan.aggSpecs[i].operand; op != nil && op.kind == reColumn && !need(op.index) {
			return nil, nil, false, nil
		}
	}
	if len(gset.keyCols) == 1 && !need(gset.keyCols[0]) {
		return nil, nil, false, nil
	}

	// Determine the scan bound exactly as materializeRel does: a PK-range bound, or the full scan. An
	// empty bound (a contradictory PK predicate) admits no rows — skip the scan entirely (0 pages/rows),
	// matching materializeRel's `if !empty` guard.
	scan := true
	b := unboundedBound()
	if sb := plan.phys.relBounds[0]; sb != nil && sb.pk != nil {
		var empty bool
		if b, empty = db.buildKeyBound(sb.pk, env.params, env.outer, nil); empty {
			scan = false
		}
	}

	grouped := len(gset.keyCols) == 1
	zone := db.session.timeZone

	// Fold each scanned row's touched columns straight into its accumulator during ONE tree walk — no
	// per-column lane is materialized, so a whole-table / single-int-key aggregate is O(1) memory
	// (float.md §7, packed-leaf.md §11). The WHERE compiles once into span atoms over the stored bytes
	// plus generic predicates over a reusable scratch row (Track A4) — byte-identical survivors +
	// operator_eval to the row path's evalFilter.
	var accs []*acc // whole-table grand-total accumulators (grouped == false)
	if !grouped {
		accs = newAccsForSpecs(plan.aggSpecs, zone)
	}
	var groups []laneGroup // grouped == true: one per group, in scan-order-of-first-appearance
	groupIdx := make(map[int64]int)
	nullGI := -1
	keyIdx := 0
	if grouped {
		keyIdx = gset.keyCols[0]
	}
	// On a spill-capable host a grouped fold is bounded (memory.md §6.6.1): the group table's size is
	// tracked — and charged — on the state map's schedule (a fixed part per group, plus each decimal
	// sum's and MIN/MAX value's own bytes as they change), and past work_mem, or once the account
	// turns a reservation away, the table is handed over to the spill structures, into which the rest
	// of the scan folds.
	bounded := grouped && db.boundedBlockingEligible(plan)
	budget := int64(db.session.workMem)
	var groupBase int64
	variable := make([]bool, len(plan.aggSpecs))
	if bounded {
		groupBase = laneGroupBaseBytes(plan.aggSpecs, zone)
		for s := range plan.aggSpecs {
			variable[s] = accIsVariable(&plan.aggSpecs[s])
		}
	}
	var resident int64
	rejected := false
	var spilled *laneGroupSpill
	var span *laneFilter
	if plan.filter != nil {
		span = compileLaneFilter(plan.filter, store.colTypes, mask, env)
	}
	scratch := newScratchRow(len(mask))
	// How each aggregate reads its operand, decided once: COUNT(col) reads only nullness (Track A4); a
	// variable-width operand beside a filter reuses the value the filter resolved, if any; any other
	// operand reads straight from the leaf.
	reuse := laneReuse(store.colTypes, span != nil)
	opRead := make([]operandRead, len(plan.aggSpecs))
	opCol := make([]int, len(plan.aggSpecs))
	operandCols := make([][]int, len(plan.aggSpecs))
	exprOps := hasExpressionOperand(plan)
	for s := range plan.aggSpecs {
		op := plan.aggSpecs[s].operand
		switch {
		case op == nil:
			opRead[s] = operandNone
		case op.kind != reColumn:
			// Evaluated over the scratch row: the columns it reads, resolved once per record (shared
			// with a filter that read them).
			opRead[s] = operandExpr
			reads := make([]bool, len(mask))
			collectTouched(op, 0, reads)
			for c := range mask {
				if reads[c] && mask[c] {
					operandCols[s] = append(operandCols[s], c)
				}
			}
		case plan.aggSpecs[s].plan == planCount:
			opRead[s], opCol[s] = operandNullness, op.index
		case reuse[op.index]:
			opRead[s], opCol[s] = operandReuse, op.index
		default:
			opRead[s], opCol[s] = operandLeaf, op.index
		}
	}
	// A decimal SUM/AVG over a decimal column read straight from the leaf folds a short value from
	// its stored bytes, with no Value built for it (packed-leaf.md §11 Track A5).
	decFold := make([]bool, len(plan.aggSpecs))
	for s := range plan.aggSpecs {
		if p := plan.aggSpecs[s].plan; (p == planSumDecimal || p == planAvg) && opRead[s] == operandLeaf {
			ty := store.colTypes[opCol[s]]
			decFold[s] = !ty.Composite && ty.Elem == nil && ty.RangeElem == nil && ty.Scalar.IsDecimal()
		}
	}
	unitPages, unitSlabs := 0, 0
	// The group table: the whole-table group's entry up front and an unbounded grouped entry as each
	// group is created (memory.md §6.2), returned after the last group is finalized; the bounded
	// table's state-map measure (§6.6.1), returned at its hand-over or after its last finalize.
	charge := meter.stateCharge()
	defer charge.releaseAll()
	if !grouped && charge.active() {
		if err := charge.reserve(meter, memEntryBytes(nil)); err != nil {
			return nil, nil, false, err
		}
	}

	nsurv := 0
	rowCount, nodes := 0, 0
	visited := 0
	// What the folds themselves have charged (decimal work, long comparisons), and whether the scan
	// stopped on a filter error — for the exact accounting of a failed statement below.
	var foldLive int64
	filterFailed := false
	if scan {
		// operand reads aggregate s's operand for record i of n.
		operand := func(n *pnode, i, s int) (Value, error) {
			v := NullValue()
			switch opRead[s] {
			case operandNullness:
				if !n.colIsNull(i, opCol[s]) {
					v = BoolValue(true)
				}
			case operandLeaf:
				return n.colAtResolved(i, opCol[s])
			case operandReuse:
				return scratch.read(n, i, opCol[s])
			case operandExpr:
				for _, c := range operandCols[s] {
					if e := scratch.ensure(n, i, c); e != nil {
						return v, e
					}
				}
				return plan.aggSpecs[s].operand.eval(scratch.vals, env, meter)
			}
			return v, nil
		}
		// foldOperand folds record i's operand of aggregate s into a.
		foldOperand := func(a *acc, n *pnode, i, s int) error {
			if decFold[s] {
				var buf [2]uint32
				if d, null, ok := n.smallDecimalAt(i, opCol[s], &buf); ok {
					if null {
						return nil
					}
					return a.addDecimal(d, meter)
				}
			}
			v, e := operand(n, i, s)
			if e != nil {
				return e
			}
			return a.fold(v, meter)
		}
		var werr error
		rowCount, nodes, werr = store.rows.foldScan(b, store.leafSrc(), func(n *pnode, i int) error {
			if visited&255 == 0 {
				if err := meter.Checkpoint(); err != nil {
					return err
				}
				// Under a ceiling this fold is an attempt (cost.md §6.1): once the cost it has earned
				// but not yet charged would trip, the statement cannot complete, so stop within this
				// batch. The caller discards the attempt and its error.
				pending := costs.StorageRowRead*int64(visited) + costs.PageRead*int64(unitPages) +
					costs.ValueDecompress*int64(unitSlabs) + costs.AggregateAccumulate*int64(nsurv)*int64(len(plan.aggSpecs))
				if meter.pendingTrips(pending) {
					return newError(CostLimitExceeded, "the columnar fold reached the cost limit")
				}
			}
			visited++
			if spillable {
				p, d := n.recordScanUnitsCols(i, unitCols, store.colTypes, store.cap, mask)
				unitPages += p
				unitSlabs += d
			}
			if span != nil {
				fv, e := span.keep(n, i, scratch, env, meter)
				if e != nil {
					filterFailed = true
					return e
				}
				if !fv {
					return nil
				}
			} else if exprOps {
				scratch.nextRecord() // no filter started a scratch record; an expression operand fills one
			}
			nsurv++
			foldStart := meter.Accrued
			if !grouped {
				for s := range opRead {
					if e := foldOperand(accs[s], n, i, s); e != nil {
						return e
					}
				}
				foldLive += meter.Accrued - foldStart
				return nil
			}
			// A NULL integer key buckets into one sentinel group (the value-canonical key groups all
			// NULLs together — matching the scalar path).
			kv, e := n.colAt(i, keyIdx)
			if e != nil {
				return e
			}
			if spilled != nil {
				// Handed over: the per-row step blockingAggregate runs — load the group's state, fold
				// each aggregate, store it back. An integer key charges no hash work.
				state, e := spilled.state(kv, plan.aggSpecs, zone)
				if e != nil {
					return e
				}
				for s := range opRead {
					v, e := operand(n, i, s)
					if e != nil {
						return e
					}
					if e := laneGroupSpillFold(state, s, plan.aggSpecs[s], v, zone, meter); e != nil {
						return e
					}
				}
				foldLive += meter.Accrued - foldStart
				return spilled.put(kv, state)
			}
			gi, found := nullGI, nullGI >= 0
			if kv.Kind != ValNull {
				gi, found = groupIdx[kv.Int]
			}
			if !found {
				if bounded {
					// The bounded table reserves what it measures (memory.md §6.6.1).
					if !charge.tryReserveDirect(groupBase) {
						rejected = true
					}
				} else if charge.active() {
					if err := charge.reserve(meter, memEntryBytes([]Value{kv})); err != nil {
						return err
					}
				}
				gi = len(groups)
				if kv.Kind == ValNull {
					nullGI = gi
				} else {
					groupIdx[kv.Int] = gi
				}
				groups = append(groups, laneGroup{key: kv, accs: newAccsForSpecs(plan.aggSpecs, zone)})
				resident += groupBase
			}
			a := groups[gi].accs
			for s := range opRead {
				if !variable[s] {
					if e := foldOperand(a[s], n, i, s); e != nil {
						return e
					}
					continue
				}
				before := accVariableBytes(a[s])
				if e := foldOperand(a[s], n, i, s); e != nil {
					return e
				}
				delta := accVariableBytes(a[s]) - before
				resident += delta
				if delta > 0 {
					if !charge.tryReserveDirect(delta) {
						rejected = true
					}
				} else if delta < 0 {
					charge.release(-delta)
				}
			}
			foldLive += meter.Accrued - foldStart
			// Past work_mem, or once the account turns a reservation away, the table hands over like
			// any spill structure: its whole charge is returned first.
			if bounded && (resident > budget || rejected) {
				charge.releaseAll()
				sp, e := db.laneGroupHandover(groups, own)
				if e != nil {
					return e
				}
				spilled = sp
				groups, groupIdx, nullGI = nil, nil, -1
			}
			return nil
		})
		if werr != nil {
			if e, ok := werr.(*EngineError); ok && e.Code() == "57014" {
				// Bulk accrual must not erase work already done by a canceled scan.
				meter.Charge(costs.PageRead*int64(nodes+unitPages) + costs.ValueDecompress*int64(unitSlabs) + costs.StorageRowRead*int64(visited))
				meter.Charge(costs.AggregateAccumulate * int64(nsurv) * int64(len(plan.aggSpecs)))
			} else if filterFailed && meter.canBatchCosts() && !exprOps {
				// A filter error leaves what the row path would have accrued at the same error (cost.md
				// §6.2). The row path scans the whole bound, then filters, then folds: so the whole scan
				// block and every row read are owed, and nothing the folds charged. (Under a ceiling, or
				// with an expression operand, this run is an attempt the caller discards.)
				meter.refund(foldLive)
				if err := store.chargeWholeBound(b, mask, meter); err != nil {
					return nil, nil, false, err
				}
			}
			return nil, nil, false, werr
		}
	}

	// Charge the identical cost totals: the scan block (page_read × (nodes + chain pages) — the same
	// node visits; value_decompress × slabs; storage_row_read × rowCount) and aggregate_accumulate once
	// per (survivor × spec), in bulk (charge order is invisible: the caller accepts the lane's result
	// only where the totals decide — cost.md §6.1). The filter's operator_eval was charged per scanned
	// row inside foldScan.
	meter.Charge(costs.PageRead*int64(nodes+unitPages) + costs.ValueDecompress*int64(unitSlabs) + costs.StorageRowRead*int64(rowCount))
	meter.Charge(costs.AggregateAccumulate * int64(nsurv) * int64(len(plan.aggSpecs)))

	if spilled != nil {
		out, e := db.laneGroupFinish(spilled, plan.aggSpecs, own)
		if e != nil {
			return nil, nil, false, e
		}
		return nil, out, true, nil
	}
	if grouped {
		out := make([]storedRow, 0, len(groups))
		for i := range groups {
			srow, e := finalizeGroup([]Value{groups[i].key}, groups[i].accs)
			if e != nil {
				return nil, nil, false, e
			}
			out = append(out, srow)
		}
		charge.releaseAll()
		if bounded {
			// On a spill-capable host the groups leave as a spool, like the bounded-spill lane's own
			// (memory.md §5.4): it spills where a row buffer would fail.
			spool := own.spool()
			for _, r := range out {
				if e := spool.push(r); e != nil {
					return nil, nil, false, e
				}
			}
			return nil, spool, true, nil
		}
		return out, nil, true, nil
	}
	srow, e := finalizeGroup(nil, accs)
	if e != nil {
		return nil, nil, false, e
	}
	charge.releaseAll()
	return []storedRow{srow}, nil, true, nil
}

// operandRead is how the columnar aggregate lane reads one aggregate's operand per record
// (packed-leaf.md §11 Track A4), decided once per statement.
type operandRead uint8

const (
	operandNone     operandRead = iota // COUNT(*): no operand
	operandNullness                    // COUNT(col): only the column's nullness
	operandLeaf                        // read straight from the leaf (a deferred value resolved)
	operandReuse                       // a variable-width column beside a filter: reuse its resolved value
	operandExpr                        // an expression, evaluated over the scratch row
)

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
	if len(plan.phys.relBounds) > 0 && plan.phys.relBounds[0].needsEagerScan() {
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

// projectColumnar runs the columnar projection lane for a vectorizedProjectEligible plan (packed-leaf.md
// §11 Tracks A2–A4): it walks the single base relation's bound once, evaluates any WHERE per record as a
// span filter (laneFilter — atoms on the stored bytes, generic predicates over a reusable scratch row),
// gathers the projected columns of the SURVIVING records into dense per-column lanes (never a full-width
// storedRow; a non-survivor's variable-width values are never decoded), charges the identical scan cost
// block, and returns an emitColumnar emitter that builds each output row from the lanes on emission.
// Returns ok=false (declining to the caller's row path) for an in-memory store (its Decoded leaves share
// rows zero-copy, so a lane gather would only add allocation), or a projection column that is out of
// range / unmasked (a safety net, never expected — a projected column is touched, hence masked).
// Cost-neutral by construction: same page_read (same node visits, plus each admitted record's chain
// pages) / value_decompress / storage_row_read (× rowCount) / operator_eval (the filter over each scanned
// record) as the row path, then row_produced per emitted row charged by the emitColumnar drive — exactly
// the emitProject drive over a bare-column projection.
func (db *engine) projectColumnar(plan *selectPlan, env *evalEnv, meter *costMeter) (emitter, bool, error) {
	rel := &plan.rels[0]
	store := db.lkpStoreScoped(rel.db, rel.tableName)
	if store == nil {
		return emitter{}, false, nil
	}
	// Packed-page stores include committed memory databases (see aggColumnar).
	if store.paging == nil {
		return emitter{}, false, nil
	}
	mask := plan.relMasks[0]
	// Each projected column must be a valid, masked table ordinal — else its gathered lane would be nil.
	// For a single base table a projection's reColumn index is a 0-based table ordinal in [0, K); a
	// projected column is always touched (hence masked), so this holds — the check also declines a
	// (never-expected) synthetic slot or non-zero relation offset.
	projCols := make([]int, len(plan.projections))
	gather := make([]bool, len(mask))
	for i, p := range plan.projections {
		idx := p.index
		if idx < 0 || idx >= len(mask) || !mask[idx] {
			return emitter{}, false, nil
		}
		projCols[i] = idx
		gather[idx] = true
	}
	var gatherCols []int
	for c := range gather {
		if gather[c] {
			gatherCols = append(gatherCols, c)
		}
	}
	// A touched variable-width column adds each admitted record's large-value units (chain pages,
	// decompress slabs) to the scan block, read off its stored tag (Track A4).
	spillable := anySpillableMasked(store.colTypes, mask)
	unitCols := spillUnitCols(store.colTypes, mask)

	// Determine the scan bound exactly as materializeRel does: a PK-range bound, or the full scan. An
	// empty bound (a contradictory PK predicate) admits no rows — skip the scan entirely (0 pages/rows).
	scan := true
	b := unboundedBound()
	if len(plan.phys.relBounds) > 0 {
		if sb := plan.phys.relBounds[0]; sb != nil && sb.pk != nil {
			var empty bool
			if b, empty = db.buildKeyBound(sb.pk, env.params, env.outer, nil); empty {
				scan = false
			}
		}
	}
	var span *laneFilter
	if plan.filter != nil {
		span = compileLaneFilter(plan.filter, store.colTypes, mask, env)
	}
	scratch := newScratchRow(len(mask))
	reuse := laneReuse(store.colTypes, span != nil)
	cols := make([][]Value, len(mask))
	rowCount, nodes, unitPages, unitSlabs, nEmit := 0, 0, 0, 0, 0
	visited := 0
	// Whether the scan stopped on a filter error (for the exact accounting below).
	filterFailed := false
	if scan {
		var err error
		rowCount, nodes, err = store.rows.foldScan(b, store.leafSrc(), func(n *pnode, i int) error {
			if visited&255 == 0 {
				if err := meter.Checkpoint(); err != nil {
					return err
				}
				// Under a ceiling this gather is an attempt (cost.md §6.1): stop within a batch of the
				// cost it has earned but not yet charged reaching the ceiling.
				pending := costs.StorageRowRead*int64(visited) + costs.PageRead*int64(unitPages) + costs.ValueDecompress*int64(unitSlabs)
				if meter.pendingTrips(pending) {
					return newError(CostLimitExceeded, "the columnar gather reached the cost limit")
				}
			}
			visited++
			if spillable {
				p, d := n.recordScanUnitsCols(i, unitCols, store.colTypes, store.cap, mask)
				unitPages += p
				unitSlabs += d
			}
			if span != nil {
				keep, err := span.keep(n, i, scratch, env, meter)
				if err != nil {
					filterFailed = true
					return err
				}
				if !keep {
					return nil
				}
			}
			for _, c := range gatherCols {
				var v Value
				var err error
				if reuse[c] {
					v, err = scratch.read(n, i, c)
				} else {
					v, err = readLeafColumn(n, i, c)
				}
				if err != nil {
					return err
				}
				cols[c] = append(cols[c], v)
			}
			nEmit++
			return nil
		})
		if err != nil {
			if e, ok := err.(*EngineError); ok && e.Code() == "57014" {
				meter.Charge(costs.PageRead*int64(nodes+unitPages) + costs.ValueDecompress*int64(unitSlabs) + costs.StorageRowRead*int64(visited))
			} else if filterFailed && meter.canBatchCosts() {
				// A filter error leaves what the row path would have accrued at the same error (cost.md
				// §6.2): it scans the whole bound before it filters.
				if cerr := store.chargeWholeBound(b, mask, meter); cerr != nil {
					return emitter{}, false, cerr
				}
			}
			return emitter{}, false, err
		}
	}
	// Charge the scan cost block identically to materializeRel + scanSource: page_read × (nodes + chain
	// pages), value_decompress × slabs, storage_row_read × rowCount. On the unmetered lane (the caller
	// gates) this bulk charge reproduces the scanSource's per-row accrual. The filter's operator_eval
	// was charged per scanned record inside the walk.
	meter.Charge(costs.PageRead*int64(nodes+unitPages) + costs.ValueDecompress*int64(unitSlabs) + costs.StorageRowRead*int64(rowCount))
	// The gathered lanes are operator state, reserved once the bulk gather completes (memory.md §6.5)
	// and returned when the emission completes.
	charge := meter.stateCharge()
	if charge.active() {
		var bytes int64
		for _, lane := range cols {
			bytes += memKeyBytes(lane)
		}
		if err := charge.reserve(meter, bytes); err != nil {
			return emitter{}, false, err
		}
	}
	return emitter{cols: cols, projCols: projCols, start: 0, end: int64(nEmit), mode: emitColumnar, colCharge: charge}, true, nil
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
func newAccsForSpecs(specs []aggSpec, zone ZoneRef) []*acc {
	accs := make([]*acc, len(specs))
	for i := range specs {
		accs[i] = newAccFromSpec(specs[i], zone)
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
func (db *engine) groupByIntKey(plan *selectPlan, gset *groupSetPlan, survivors []storedRow, env *evalEnv, meter *costMeter) ([]storedRow, error) {
	keyIdx := gset.keyCols[0]
	type vgroup struct {
		key  Value
		accs []*acc
	}
	var groups []vgroup
	index := make(map[int64]int)
	nullGI := -1
	// The group table's entries (memory.md §6.2): all groups are finalized before any group row is
	// admitted, so the whole table is returned after the last finalize.
	charge := meter.stateCharge()
	defer charge.releaseAll()

	meter.Charge(costs.AggregateAccumulate * int64(len(survivors)) * int64(len(plan.aggSpecs)))
	for ri, r := range survivors {
		if ri&255 == 0 {
			if err := meter.Checkpoint(); err != nil {
				return nil, err
			}
		}
		kv := r[keyIdx]
		var gi int
		if kv.Kind == ValNull {
			if nullGI < 0 {
				if charge.active() {
					if err := charge.reserve(meter, memEntryBytes([]Value{NullValue()})); err != nil {
						return nil, err
					}
				}
				nullGI = len(groups)
				groups = append(groups, vgroup{key: kv, accs: newAccsForSpecs(plan.aggSpecs, db.session.timeZone)})
			}
			gi = nullGI
		} else {
			var ok bool
			if gi, ok = index[kv.Int]; !ok {
				if charge.active() {
					if err := charge.reserve(meter, memEntryBytes([]Value{kv})); err != nil {
						return nil, err
					}
				}
				gi = len(groups)
				index[kv.Int] = gi
				groups = append(groups, vgroup{key: kv, accs: newAccsForSpecs(plan.aggSpecs, db.session.timeZone)})
			}
		}
		// Fold each aggregate into this row's group. acc.fold charges nothing for the eligible plans
		// (COUNT/SUM-int/float/MIN/MAX) — the aggregate_accumulate was charged in bulk above — and a
		// bare-column operand's value is r[operand.index] (== operand.eval, which charges 0). COUNT(*)
		// has no operand; fold ignores the value.
		accs := groups[gi].accs
		for i := range plan.aggSpecs {
			v := NullValue()
			if op := plan.aggSpecs[i].operand; op != nil && op.kind == reColumn {
				v = r[op.index]
			} else if op != nil {
				// An expression operand, evaluated over the survivor's row (charging its operators, as
				// the row path does per survivor).
				var ferr error
				if v, ferr = op.eval(r, env, meter); ferr != nil {
					return nil, ferr
				}
			}
			if ferr := accs[i].fold(v, meter); ferr != nil {
				return nil, ferr
			}
		}
	}

	out := make([]storedRow, 0, len(groups))
	for i := range groups {
		if i&255 == 0 {
			if err := meter.Checkpoint(); err != nil {
				return nil, err
			}
		}
		srow, ferr := finalizeGroup([]Value{groups[i].key}, groups[i].accs)
		if ferr != nil {
			return nil, ferr
		}
		out = append(out, srow)
	}
	charge.releaseAll()
	return out, nil
}

// foldAggBatch folds one WHOLE-TABLE aggregate over the survivor rows into acc, reconstructing
// exactly the state the scalar fold (executor.go acc.fold) would leave so acc.finalize is unchanged.
// It charges aggregate_accumulate once per survivor (the scalar loop charges it per (row × spec)),
// bulk on the unmetered lane. COUNT/SUM-int stride the int lane via a colBatch; MIN/MAX and the
// float SUM/AVG folds run over the raw Values (valueCmp, or a collated MIN/MAX's sort-key compare /
// floatSum.add — matching the scalar arm exactly).
func foldAggBatch(a *acc, spec *aggSpec, survivors []storedRow, env *evalEnv, meter *costMeter) error {
	meter.Charge(costs.AggregateAccumulate * int64(len(survivors)))
	if err := meter.Checkpoint(); err != nil {
		return err
	}
	if op := spec.operand; op != nil && op.kind != reColumn {
		// An expression operand: evaluated over each survivor's row (charging its operators, as the
		// row path does per survivor) and folded through the shared acc.fold.
		for i := range survivors {
			if i&255 == 0 {
				if err := meter.Checkpoint(); err != nil {
					return err
				}
			}
			v, err := op.eval(survivors[i], env, meter)
			if err != nil {
				return err
			}
			if err := a.fold(v, meter); err != nil {
				return err
			}
		}
		return nil
	}
	switch spec.plan {
	case planCountStar:
		a.count += int64(len(survivors))
	case planCount:
		b := extractIntColumn(survivors, spec.operand.index)
		for i := 0; i < b.n; i++ {
			if i&255 == 0 {
				if err := meter.Checkpoint(); err != nil {
					return err
				}
			}
			if !b.nulls[i] {
				a.count++
			}
		}
	case planSumInt:
		b := extractIntColumn(survivors, spec.operand.index)
		for i := 0; i < b.n; i++ {
			if i&255 == 0 {
				if err := meter.Checkpoint(); err != nil {
					return err
				}
			}
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
			if i&255 == 0 {
				if err := meter.Checkpoint(); err != nil {
					return err
				}
			}
			v := survivors[i][idx]
			if v.Kind != ValNull {
				a.floatSum.add(v)
			}
		}
	case planMin, planMax:
		idx := spec.operand.index
		for i := range survivors {
			if i&255 == 0 {
				if err := meter.Checkpoint(); err != nil {
					return err
				}
			}
			v := survivors[i][idx]
			if v.Kind == ValNull {
				continue
			}
			if !a.hasCur {
				a.cur, a.hasCur = v, true
				continue
			}
			// A text MIN/MAX under a collation compares sort keys, exactly like acc.fold
			// (spec/design/collation.md §1).
			c, err := collatedOrValueCmp(a.collation, a.cur, v)
			if err != nil {
				return err
			}
			keepCur := (spec.plan == planMin && c <= 0) || (spec.plan == planMax && c >= 0)
			if !keepCur {
				a.cur = v
			}
		}
	case planSumDecimal, planAvg:
		// The decimal folds run through the shared acc.fold, in scan order, so their running-sum
		// decimal_work is charged exactly as the scalar fold charges it (cost.md §3).
		idx := spec.operand.index
		for i := range survivors {
			if i&255 == 0 {
				if err := meter.Checkpoint(); err != nil {
					return err
				}
			}
			if err := a.fold(survivors[i][idx], meter); err != nil {
				return err
			}
		}
	default:
		panic("foldAggBatch: ineligible aggregate plan")
	}
	return nil
}

// ---- The bounded group table, its hand-over, and the lane's ceiling attempt ----

// The columnar aggregate lane's bounded group table and its hand-over to the spill structures
// (spec/design/memory.md §5.4, §6.6.1; packed-leaf.md §11 Track A5), and the lane's attempt under a
// cost ceiling (spec/design/cost.md §6.1). Mirrors the Rust core's blocking_spill.rs /
// access_encode.rs / exec_emit.rs: which lane a statement takes decides what it reserves under a
// query-memory budget, so the choice and every reservation here are cross-core contract.

// laneTooling is a session's aggregate-lane tooling, read by the tests: how many grouped columnar
// folds handed their group table over to the spill structures, how many attempts the lane made under
// a cost ceiling and how many of those fell back to the row path, and the switch that turns the
// attempt off so a test can compare it against the row path alone (cost.md §6.1). Per session, so
// parallel tests do not see each other's.
type laneTooling struct {
	handovers  uint64
	attempts   uint64
	fallbacks  uint64
	ceilingOff bool
}

// laneTools returns the session's lane tooling (a throwaway for an engine built without a session).
func (db *engine) laneTools() *laneTooling {
	if db.session.lane == nil {
		return &laneTooling{}
	}
	return db.session.lane
}

// discardSafe reports whether evaluating e leaves nothing behind, so a lane attempt that evaluated it
// can be discarded and the statement run again on the row path (cost.md §6.1). A deliberate
// allowlist, identical in every core: columns, constants, parameters, and the operators over them
// that read no entropy, clock, sequence, host code or subquery and keep no per-node state. Anything
// else — every function call, a regex (its compile is charged once per node) — is not safe, and such
// a statement keeps the row path under a ceiling.
func discardSafe(e *rExpr) bool {
	if e == nil {
		return true
	}
	switch e.kind {
	case reColumn, reParam, reConstInt, reConstBool, reConstText, reConstDecimal, reConstFloat32,
		reConstFloat64, reConstBytea, reConstUuid, reConstTimestamp, reConstTimestamptz, reConstDate,
		reConstInterval, reConstNull:
		return true
	case reNot, reCast, reNeg, reIsNull:
		return discardSafe(e.operand)
	case reAnd, reOr, reArith, reCompare, reDistinct, reLike:
		return discardSafe(e.lhs) && discardSafe(e.rhs)
	case reCase:
		for i := range e.caseArms {
			if !discardSafe(e.caseArms[i].cond) || !discardSafe(e.caseArms[i].result) {
				return false
			}
		}
		return discardSafe(e.caseEls)
	case reCoalesce, reGreatestLeast, reNullIf, reDateTrunc:
		for _, a := range e.sargs {
			if !discardSafe(a) {
				return false
			}
		}
		return true
	case reScalarFunc:
		// A built-in call, unless the catalog marks it volatile: entropy, the per-call clock, the
		// sequence functions (volatileScalarFuncs). now() and the other statement-clock reads are
		// stable: the clock is resolved once per statement and survives a discarded attempt.
		if slices.Contains(volatileScalarFuncs, e.sfunc) {
			return false
		}
		for _, a := range e.sargs {
			if !discardSafe(a) {
				return false
			}
		}
		return true
	case reDateClock:
		return true
	case reExtract, reDateConvert, reCasing:
		return discardSafe(e.operand)
	case reAtTimeZone:
		return discardSafe(e.lhs) && discardSafe(e.rhs)
	case reInValues:
		// A list whose values are key points of the left operand carries its subquery's bookkeeping
		// (the Rust core's FoldedPoints); any other folded list is plain values.
		return !e.elemExact && !e.elemPromoted && discardSafe(e.lhs)
	default:
		return false
	}
}

// volatileScalarFuncs is the built-in scalar functions the catalog marks volatile
// (spec/functions/catalog.toml), which a discarded attempt cannot take back: entropy, the per-call
// clock, and the sequence functions.
var volatileScalarFuncs = []scalarFunc{sfUuidv4, sfUuidv7, sfClockTimestamp, sfNextval, sfCurrval, sfSetval, sfLastval}

// hasExpressionOperand reports whether any of plan's aggregates folds an expression rather than a
// bare column. Such a lane run interleaves filter and operand evaluation where the row path filters
// every row first, so it is always an attempt the row path replaces on any error (cost.md §6.1).
func hasExpressionOperand(plan *selectPlan) bool {
	for i := range plan.aggSpecs {
		if op := plan.aggSpecs[i].operand; op != nil && op.kind != reColumn {
			return true
		}
	}
	return false
}

// laneDiscardSafe reports whether every expression the aggregate lane evaluates for plan — its WHERE
// and each ORDER BY expression over its groups — is safe to discard.
func laneDiscardSafe(plan *selectPlan) bool {
	if !discardSafe(plan.filter) {
		return false
	}
	for _, e := range plan.orderExprs {
		if !discardSafe(e) {
			return false
		}
	}
	return true
}

// boundedWholeAggregate reports whether plan is a whole-table aggregate over one page-backed base
// table: the columnar lane's whole-table fold holds constant state, so it keeps the lane where the
// bounded-spill lane would otherwise take the statement (memory.md §5.4).
func (db *engine) boundedWholeAggregate(plan *selectPlan) bool {
	if len(plan.rels) != 1 || len(plan.groupSets) != 1 || len(plan.groupSets[0].keyCols) != 0 {
		return false
	}
	return db.laneBaseRelPaged(plan)
}

// handoverGroupedAggregate reports whether the columnar lane may run grouped plan on a spill-capable
// host, handing its group table over to the spill structures when it outgrows work_mem or the
// query-memory account turns a reservation away (memory.md §5.4): one grouping set with one key over
// one page-backed base table.
func (db *engine) handoverGroupedAggregate(plan *selectPlan) bool {
	if len(plan.rels) != 1 || len(plan.groupSets) != 1 || len(plan.groupSets[0].keyCols) != 1 {
		return false
	}
	return db.laneBaseRelPaged(plan)
}

func (db *engine) laneBaseRelPaged(plan *selectPlan) bool {
	rel := &plan.rels[0]
	if rel.srf != nil || rel.derived != nil || rel.cte != nil {
		return false
	}
	store := db.lkpStoreScoped(rel.db, rel.tableName)
	return store != nil && store.paging != nil
}

// aggregateLaneEligible is the dispatch gate of the vectorized / columnar aggregate lane (memory.md
// §5.4): the shape rules, no EXPLAIN ANALYZE profile, a statement the bounded-spill lane would not
// take (or a whole-table / single-key grouped fold over a page-backed table), and an ORDER BY only on
// the bounded grouped form, which sorts its groups.
func (db *engine) aggregateLaneEligible(plan *selectPlan) bool {
	if db.explainActual != nil || !db.vectorizedAggEligible(plan) {
		return false
	}
	bounded := db.boundedBlockingEligible(plan)
	handover := db.handoverGroupedAggregate(plan)
	// An ORDER BY over the result is part of the shape in every form (memory.md §5.4): the bounded
	// form sorts through the bounded-spill lane's stage, the unbounded form as the eager lane does.
	return !bounded || db.boundedWholeAggregate(plan) || handover
}

// freshGroupState is a new group's state-map value, as blockingAggregate builds it: its creation
// ordinal, then each aggregate's packed fresh accumulator.
func freshGroupState(specs []aggSpec, ordinal int64, zone ZoneRef) storedRow {
	state := make(storedRow, 0, 1+len(specs))
	state = append(state, IntValue(ordinal))
	for _, spec := range specs {
		state = append(state, CompositeValue(blockingAccRow(newAccFromSpec(spec, zone))))
	}
	return state
}

// laneGroupBaseBytes is what one group with an integer (or NULL) key holds in a resident state map
// before any fold (memory.md §6.6): ENTRY + key + state.
func laneGroupBaseBytes(specs []aggSpec, zone ZoneRef) int64 {
	return memMapEntryBytes([]Value{NullValue()}, freshGroupState(specs, 0, zone))
}

// accVariableBytes is the part of an accumulator's packed size that changes as it folds: a decimal
// sum's digit groups, a MIN/MAX's held value. Every other accumulator the lane folds packs to a fixed
// size.
func accVariableBytes(a *acc) int64 {
	switch a.plan {
	case planSumDecimal, planAvg:
		return memoryDecimalGroup * int64(a.sumDec.codecGroupCount())
	case planMin, planMax:
		if a.hasCur {
			return memPayloadBytes(a.cur)
		}
	}
	return 0
}

// accIsVariable reports whether spec's accumulator has an accVariableBytes part.
func accIsVariable(spec *aggSpec) bool {
	switch spec.plan {
	case planSumDecimal, planAvg, planMin, planMax:
		return true
	}
	return false
}

// laneGroupSpill is the columnar grouped lane's group table once it has handed over: the spill
// executor's own structures, which spill by their own rule.
type laneGroupSpill struct {
	groups *stateMap // group key → [creation ordinal, packed accumulator…]
	keys   *rowSpool // the group keys in creation order — the emission order
}

// state returns key's state, creating the group (and recording its key for emission) when it is new.
func (s *laneGroupSpill) state(key Value, specs []aggSpec, zone ZoneRef) (storedRow, error) {
	state, ok, err := s.groups.get([]Value{key})
	if err != nil {
		return nil, err
	}
	if ok {
		return append(storedRow(nil), state...), nil
	}
	state = freshGroupState(specs, s.keys.count, zone)
	if err := s.keys.push(storedRow{key}); err != nil {
		return nil, err
	}
	return state, nil
}

// laneGroupSpillFold folds v into aggregate si of a loaded state.
func laneGroupSpillFold(state storedRow, si int, spec aggSpec, v Value, zone ZoneRef, m *costMeter) error {
	if scratchAccPlan(spec) {
		var a acc
		blockingAccInto(&a, spec, *state[si+1].composite(), zone)
		if err := a.fold(v, m); err != nil {
			return err
		}
		state[si+1] = CompositeValue(blockingAccRow(&a))
		return nil
	}
	a := blockingAccFromRow(spec, *state[si+1].composite(), zone)
	if err := a.fold(v, m); err != nil {
		return err
	}
	state[si+1] = CompositeValue(blockingAccRow(a))
	return nil
}

// put stores key's folded state back.
func (s *laneGroupSpill) put(key Value, state storedRow) error {
	return s.groups.put([]Value{key}, state)
}

// laneGroup is one group of the columnar lane's in-memory table.
type laneGroup struct {
	key  Value
	accs []*acc
}

// laneGroupHandover moves the lane's in-memory groups (in creation order) into the spill structures —
// exactly what blockingAggregate would hold after folding the same rows: each group's state is put in
// the map, then its key row is pushed to the key spool (memory.md §6.6.1).
func (db *engine) laneGroupHandover(groups []laneGroup, own *blockingOwner) (*laneGroupSpill, error) {
	db.laneTools().handovers++
	spill := &laneGroupSpill{groups: own.stateMap(), keys: own.spool()}
	for ordinal := range groups {
		g := &groups[ordinal]
		state := make(storedRow, 0, 1+len(g.accs))
		state = append(state, IntValue(int64(ordinal)))
		for _, a := range g.accs {
			state = append(state, CompositeValue(blockingAccRow(a)))
		}
		if err := spill.groups.put([]Value{g.key}, state); err != nil {
			return nil, err
		}
		if err := spill.keys.push(storedRow{g.key}); err != nil {
			return nil, err
		}
	}
	return spill, nil
}

// laneGroupFinish finalizes a handed-over group table into its synthetic rows [key, agg…], in
// group-creation order, as a spool; the map and the key spool are discarded once it is filled.
func (db *engine) laneGroupFinish(spill *laneGroupSpill, specs []aggSpec, own *blockingOwner) (*rowSpool, error) {
	zone := db.session.timeZone
	output := own.spool()
	err := spill.keys.each(func(key storedRow) error {
		state, _, err := spill.groups.get(key)
		if err != nil {
			return err
		}
		out := make(storedRow, 0, len(state))
		out = append(out, key[0])
		for si := range specs {
			v, err := blockingAccFromRow(specs[si], *state[si+1].composite(), zone).finalize()
			if err != nil {
				return err
			}
			out = append(out, v)
		}
		return output.push(out)
	})
	if err != nil {
		return nil, err
	}
	spill.keys.close()
	spill.groups.close()
	return output, nil
}
