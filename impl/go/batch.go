package jed

// Vectorized batch execution (Go-core-internal; CLAUDE.md §2 "execution strategy is a per-core
// free choice"). Stage 1 of the PAX + vectorization program: a whole-table integer aggregate
// (SUM/COUNT/MIN/MAX with no GROUP BY) folds a decoded column in a tight loop instead of building
// one transient Value per row and dispatching through the row-at-a-time group machinery
// (executor.go execSelectEmit's isAgg branch). Value stays the fallback currency — any shape this
// file does not specialize takes the existing eager path, byte-identical by construction.
//
// The load-bearing invariant is that BOTH the result multiset AND the deterministic cost
// (CLAUDE.md §8/§13) stay byte-identical to the scalar path: the conformance corpus asserts
// `# cost:` on every core, so a cost divergence fails loudly. Two properties keep it identical:
//   - The scan + WHERE cost is reused verbatim — the scan runs through the same materializeRel
//     (its page_read / value_decompress / storage_row_read block) and the WHERE predicate through
//     the same rExpr.eval (its operator_eval charges + 3VL survivor test). Only the group/fold
//     machinery is replaced.
//   - The fold charges aggregate_accumulate once per (survivor × aggregate) exactly like the
//     scalar loop, and reconstructs the SAME acc state, so acc.finalize yields the identical value.
//
// The path is gated to the UNMETERED lane (costMeter.unmetered — no ceiling / lifetime budget /
// cancellation armed), which is the conformance/bench case: with nothing to abort, Guard is a
// no-op, so a bulk aggregate_accumulate charge reproduces the scalar total with no per-row Guard.
// A metered query keeps the scalar path, so its deterministic abort row is unchanged.

// colBatch is a decoded integer column pulled from a set of rows for a vectorized fold. Stage 1
// fills the int lane by striding the inline scalar of each Value (Value.Int, i16/i32/i64 all live
// there) plus a per-row NULL flag. extractIntColumn is the single seam PAX (Stage 3) later turns
// into a near-memcpy from a column-major leaf page — the fold kernels below are unchanged by that.
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
// single-base-table, whole-table (no GROUP BY) SUM/COUNT/MIN/MAX with no DISTINCT / FILTER /
// HAVING / window / ORDER BY, over a full or primary-key-bounded scan. Pure plan inspection — it
// charges nothing, so a bail is free and the general path runs with identical results + cost. The
// integer restriction is exactly the aggregate plans whose acc state this file can reconstruct:
// COUNT(*) / COUNT(col) / SUM(i16|i32) / MIN / MAX (MIN/MAX fold any type via valueCmp, like the
// scalar arm). SUM(i64|decimal) → planSumDecimal, AVG, and the float folds are deferred to Stage 2.
func vectorizedAggEligible(plan *selectPlan) bool {
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
	// Whole-table aggregation: exactly one grouping set with no keys, and no grouping machinery.
	if len(plan.groupSets) != 1 || len(plan.groupSets[0].keyCols) != 0 ||
		len(plan.groupKeys) != 0 || len(plan.groupExprs) != 0 || len(plan.groupingSpecs) != 0 {
		return false
	}
	// No blocking / re-shaping operator beyond the fold (each would add cost the fast path would
	// have to mirror; deferred).
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

// vectorizedSpecEligible reports whether one aggregate is a Stage-1 integer kernel: a plain
// (non-DISTINCT, non-FILTER, non-ordered-set, non-hypothetical) COUNT(*) / COUNT(col) / SUM(i16|i32)
// / MIN(col) / MAX(col) whose operand (where it has one) is a bare column reference.
func vectorizedSpecEligible(spec *aggSpec) bool {
	if spec.distinct || spec.filter != nil || spec.osaFrac != nil || spec.hypo != nil {
		return false
	}
	switch spec.plan {
	case planCountStar:
		return spec.operand == nil
	case planCount, planSumInt, planMin, planMax:
		return spec.operand != nil && spec.operand.kind == reColumn
	default:
		return false
	}
}

// execVectorizedAgg runs a vectorizedAggEligible plan and returns a fully-formed, already-projected
// one-row result (emitter{mode: emitFinal}) — the whole-table grand total. It reuses the scalar
// scan + WHERE for exact cost + survivor determination, folds each aggregate over the survivors
// with a columnar kernel, then produces the single output row exactly as the emitProject drive
// would (guard, row_produced, projection eval). Only runs on the unmetered lane (the caller gates).
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

	// Fold each aggregate over the survivors. One acc per spec (whole-table ⇒ a single group), so
	// the finalize + projection below read them positionally, exactly like the scalar synthetic row.
	accs := make([]*acc, len(plan.aggSpecs))
	for i := range plan.aggSpecs {
		accs[i] = newAccFromSpec(plan.aggSpecs[i])
	}
	for i := range plan.aggSpecs {
		if ferr := foldAggBatch(accs[i], &plan.aggSpecs[i], survivors, meter); ferr != nil {
			return emitter{}, ferr
		}
	}

	// The synthetic grand-total row: no grouping columns, then the aggregate results in spec order,
	// then no GROUPING() values — so the projection's synthetic-slot references (index i) line up
	// with accs[i], the same layout execSelectEmit builds (len(groupKeys)==0 here).
	srow := make([]Value, 0, len(accs))
	for _, a := range accs {
		v, ferr := a.finalize()
		if ferr != nil {
			return emitter{}, ferr
		}
		srow = append(srow, v)
	}

	// Emit the one output row exactly as the emitProject drive does: guard, row_produced, then the
	// projection list (charging its operator_evals). Returned as emitFinal — already projected +
	// charged — so neither the eager nor the lazy drive charges it again.
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
	return emitter{final: [][]Value{projected}, mode: emitFinal}, nil
}

// foldAggBatch folds one integer aggregate over the survivor rows into acc, reconstructing exactly
// the state the scalar fold (executor.go acc.fold) would leave so acc.finalize is unchanged. It
// charges aggregate_accumulate once per survivor (the scalar loop charges it per (row × spec)
// before folding), bulk on the unmetered lane. COUNT/SUM stride the int lane via a colBatch;
// MIN/MAX fold the raw Values through valueCmp (type-agnostic, matching the scalar arm's compare).
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
