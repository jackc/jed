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
	// Full scan or a primary-key bound only — an index / GIN / GiST bound changes the scan
	// mechanics and residual filter, so it keeps the scalar path.
	if len(plan.relBounds) > 0 {
		if sb := plan.relBounds[0]; sb != nil && (sb.index != nil || sb.gin != nil || sb.gist != nil) {
			return false
		}
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

	// Scan the single base relation through the same path the eager executor uses, so the
	// page_read / value_decompress / storage_row_read block is charged identically (executor.go
	// materializeRel).
	rows, err := db.materializeRel(plan, 0, params, outer, rng, ctes, meter)
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
	gset := &plan.groupSets[0]
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

	// Emit under the query's LIMIT/OFFSET window, exactly as the emitProject drive does: for each
	// windowed synthetic row, Guard, row_produced, then the projection list (charging its
	// operator_evals). Only emitted rows are projected + charged (the §3 asymmetry). Returned as
	// emitFinal — already projected + charged — so neither the eager nor the lazy drive charges
	// again. windowBounds mirrors the closure in execSelectEmit (clamped in the i64 domain).
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
