package jed

import (
	"fmt"
	"math"
	"strconv"
	"strings"
)

// Result emission and post-scan projection (the back half of SELECT execution). This file holds the
// emitter (the projection/GROUP BY/HAVING/DISTINCT/window/ORDER BY driver) and execSelectEmit that
// builds it, uncorrelated-subquery folding (foldUncorrelatedIn*), the outer-reference / touched-column
// analysis used to decide correlation, and GROUPING SETS / ROLLUP / CUBE expansion plus IN/quantified
// membership and the DISTINCT row key. Row production (scans, joins, sort) is in exec_scan.go.

// emitMode is how a bufferedCursor / drainEager emits an emitter's rows (spec/design/streaming.md §4).
type emitMode int

const (
	// emitProject: the buffer rows are unprojected — emission evaluates the projection list (charging
	// its operator_evals) plus row_produced per windowed row.
	emitProject emitMode = iota
	// emitIdentity: the buffer rows are already the projected output (the DISTINCT dedup projected
	// them up front — the §3 asymmetry), so emission charges only row_produced per windowed row.
	emitIdentity
	// emitFinal: the rows are a fully-formed result (the special input-streaming paths already
	// projected AND charged them) — emission hands them out with no further charge.
	emitFinal
	// emitSorted: the streaming external sort's output, yielded LAZILY from the `sorted` pull iterator
	// (positioned past the OFFSET) — emission pulls the next sorted row, charges row_produced, and
	// evaluates the projection list per windowed row, [0, end). So the output slice is never built and a
	// caller's early exit skips the projection (and row_produced) of the rows it never pulls
	// (streaming.md §4/§7).
	emitSorted
	// emitColumnar: the columnar projection fast path (batch.go projectColumnar, packed-leaf.md §11
	// Track A2/A3). `cols` holds the pre-gathered dense per-column lanes and `projCols` the projection's
	// column indices into them; emission builds output row j as [cols[projCols[0]][lane(j)], …] — a
	// bare-column projection with no full-width storedRow, charging row_produced per windowed row exactly
	// like emitProject (a bare column ref evaluates to row[index] with zero operator_eval, so the lane
	// read is cost-identical). `sel` is the optional A3 selection vector: when non-nil, output row j maps
	// to lane position sel[j] (a filtered scan's survivors); when nil, lane position j (all rows). Lazy
	// like emitProject: a caller's early exit skips the row_produced of the rows it never pulls.
	emitColumnar
)

// emitter describes how a selectPlan's output rows are emitted (spec/design/streaming.md §4, S4): a
// SELECT runs its blocking part (scan/join/WHERE/window/sort/GROUP BY/DISTINCT) into a buffer, then
// emits a row at a time. execSelectEmit returns this so the emission can be driven EAGERLY (the
// materialized drive — execSelectPlan's drainEager builds a slice) or LAZILY (the queryValues drive —
// bufferedCursor yields it row by row, bounding output memory and short-circuiting a caller's early
// exit). Both drives charge the identical units at the identical sites (streaming.md §6).
//   - emitProject: `src` holds the UNPROJECTED rows, windowed to [start, end) — emission evaluates the
//     projection list (charging its operator_evals) + row_produced per row.
//   - emitIdentity: `final` holds the already-projected rows (the DISTINCT dedup projected them up
//     front — the §3 asymmetry), windowed to [start, end) — emission charges only row_produced.
//   - emitFinal: `final` is a fully-formed result (the special input-streaming paths already projected
//     AND charged it) — emission hands it out with no further charge.
//   - emitSorted: `sorted` is the streaming-sort output pull iterator (positioned past the OFFSET),
//     [0, end) windowed — emission pulls + projects + charges row_produced per row (streaming.md §4/§7).
//   - emitColumnar: `cols` are the dense per-column lanes and `projCols` the projection's column indices
//     into them, [start, end) windowed — emission gathers output row j from the lanes (a bare-column
//     projection, no full-width row) at lane position sel[j] (or j when sel is nil) and charges
//     row_produced per row (packed-leaf.md §11 Track A2/A3).
type emitter struct {
	src      []storedRow // emitProject: unprojected rows
	final    [][]Value   // emitIdentity / emitFinal: already-projected rows
	sorted   *sortedRows // emitSorted: the streaming-sort output pull iterator (positioned past OFFSET)
	cols     [][]Value   // emitColumnar: the dense per-column lanes (indexed by table ordinal)
	projCols []int       // emitColumnar: projection column indices into cols (one per output column)
	sel      []int32     // emitColumnar: optional A3 selection vector — output row j → lane position sel[j]
	start    int64
	end      int64
	mode     emitMode
}

// drainEager builds the full output slice from the emitter — the materialized drive
// (spec/design/streaming.md §4). The lazy queryValues drive (bufferedCursor) emits the same rows one at
// a time instead; both charge the identical units in the identical order, so totals agree (§6).
func (em emitter) drainEager(db *engine, plan *selectPlan, outer []storedRow, params []Value, ctes cteCtx, rng *stmtRng, meter *costMeter) ([][]Value, error) {
	switch em.mode {
	case emitFinal:
		return em.final, nil
	case emitSorted:
		// The streaming sort's lazy output: pull every windowed row from the `sorted` iterator,
		// charging row_produced + the projection per row — exactly the eager window loop
		// execStreamingSort ran before its output went lazy (streaming.md §4/§7).
		defer em.sorted.close() // a LIMIT/error may stop the merge early — release any undrained runs
		env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}
		out := make([][]Value, 0, em.end)
		for i := int64(0); i < em.end; i++ {
			row, ok, err := em.sorted.next()
			if err != nil {
				return nil, err
			}
			if !ok {
				break
			}
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
				return nil, err
			}
			meter.Charge(costs.RowProduced)
			projected := make([]Value, len(plan.projections))
			for j, p := range plan.projections {
				v, perr := p.eval(row, env, meter)
				if perr != nil {
					return nil, perr
				}
				projected[j] = v
			}
			out = append(out, projected)
		}
		return out, nil
	case emitIdentity:
		out := make([][]Value, 0, em.end-em.start)
		for _, row := range em.final[em.start:em.end] {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
				return nil, err
			}
			meter.Charge(costs.RowProduced)
			out = append(out, row)
		}
		return out, nil
	case emitColumnar:
		// Columnar projection (packed-leaf.md §11 Track A2/A3): gather each windowed output row from the
		// dense lanes — a bare-column projection with no full-width row — charging row_produced per row,
		// exactly the emitProject drive over a bare-column projection (whose p.eval is a zero-cost slot
		// read). A non-nil `sel` (the A3 filter's survivors) maps output row j to lane position sel[j].
		out := make([][]Value, 0, em.end-em.start)
		for j := em.start; j < em.end; j++ {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
				return nil, err
			}
			meter.Charge(costs.RowProduced)
			li := j
			if em.sel != nil {
				li = int64(em.sel[j])
			}
			projected := make([]Value, len(em.projCols))
			for k, c := range em.projCols {
				projected[k] = em.cols[c][li]
			}
			out = append(out, projected)
		}
		return out, nil
	default: // emitProject
		env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}
		out := make([][]Value, 0, em.end-em.start)
		for _, row := range em.src[em.start:em.end] {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
				return nil, err
			}
			meter.Charge(costs.RowProduced)
			projected := make([]Value, len(plan.projections))
			for i, p := range plan.projections {
				v, perr := p.eval(row, env, meter)
				if perr != nil {
					return nil, perr
				}
				projected[i] = v
			}
			out = append(out, projected)
		}
		return out, nil
	}
}

// execSelectEmit runs a selectPlan's blocking part and returns an emitter describing how to emit its
// output rows (spec/design/streaming.md §4, S4): the scan / join / WHERE / window / ORDER BY / GROUP
// BY / DISTINCT all run here (charging their cost into meter), producing either a windowed buffer
// (projected lazily on emission) or, for the special input-streaming paths, a fully-formed result. The
// caller drives the emission — eagerly (execSelectPlan, the materialized Execute path) or lazily
// (bufferedCursor, the Query path). The shared rng threads the per-statement entropy through both the
// blocking part and the (possibly deferred) projection (streaming.md §6).
func (db *engine) execSelectEmit(plan *selectPlan, outer []storedRow, params []Value, ctes cteCtx, rng *stmtRng, meter *costMeter) (emitter, error) {
	env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}

	// Vectorized single-table aggregate (batch.go, the PAX/vectorization program's executor track): a
	// SUM/COUNT/MIN/MAX/AVG with no DISTINCT / FILTER / HAVING / window / ORDER BY, either whole-table
	// or grouped by a single integer column, folds columnar / int64-bucketed instead of the
	// row-at-a-time group machinery below. Gated to the unmetered lane so a metered query's
	// deterministic abort row stays the scalar path's; results and accrued cost are byte-identical
	// either way (the conformance corpus proves both). Ineligible / metered ⇒ this is skipped and the
	// general aggregate branch runs unchanged.
	if meter.unmetered() && db.vectorizedAggEligible(plan) {
		return db.execVectorizedAgg(plan, outer, params, ctes, rng, meter)
	}

	// Columnar projection fast path (batch.go, packed-leaf.md §11 Track A2/A3): a bare-column projection
	// over a single-table full/PK-bounded scan with no ORDER BY / LIMIT / OFFSET / blocking operator
	// gathers only its touched columns into dense lanes and emits from them — never the full-width
	// storedRow the materialize path below allocates per record (the allocation dividend on a wide table).
	// A WHERE predicate (A3) is applied over the lanes into a selection vector rather than forcing the row
	// path. Gated to the unmetered lane (so a metered query's per-eval Guards stay the row path's) and to
	// file-backed stores with no spillable touched column (projectColumnar declines otherwise, falling
	// through to the identical-cost row path). Cost-neutral by construction.
	if meter.unmetered() && db.vectorizedProjectEligible(plan) {
		em, ok, err := db.projectColumnar(plan, env, meter)
		if err != nil {
			return emitter{}, err
		}
		if ok {
			return em, nil
		}
	}

	// Streaming primary-key-ordered scan (spec/design/cost.md §3): a single-table query with no
	// blocking operator beyond an ORDER BY the scan already satisfies — either no ORDER BY with a
	// LIMIT (the LIMIT short-circuit), or an ORDER BY satisfied by the table's primary-key scan order
	// (plan.pkOrdered) — streams scan→filter→project with NO sort, and with a LIMIT STOPS the scan
	// once the window is filled, so storage_row_read counts only the rows actually read (a genuine
	// early-out, not a post-hoc truncation). A non-PK-ordered ORDER BY, DISTINCT, aggregate, or join
	// must see every row, so it keeps the sort/eager path below. page_read stays the full block (the
	// bound's node count); only row reads short-circuit.
	// An index-bounded scan does not stream (cost.md §3 "index-bounded scan"): it reads
	// the full admitted set via the eager path below.
	// A set-returning relation is generated, not scanned — it takes the eager path
	// (functions.md §10); the streaming reader assumes a table store.
	// A pkOrdered DISTINCT streams too: the dedup runs in scan order (the sort elided), so it
	// short-circuits a top-N like the non-DISTINCT case. A no-ORDER-BY DISTINCT keeps the eager path.
	if streamingScanEligible(plan) {
		res, err := db.execStreamingScan(plan, env, meter, params)
		if err != nil {
			return emitter{}, err
		}
		return emitter{final: res.rows, mode: emitFinal}, nil
	}

	// Streaming secondary-index-order scan (cost.md §3 "secondary-index order"): the planner set
	// indexOrder only for a single-table, non-aggregate/window/DISTINCT, no-bound, LIMITed query
	// whose ORDER BY a B-tree index satisfies (and the PK scan does not). Walk the index +
	// point-lookup; the eager sort is elided.
	if plan.indexOrder != nil {
		res, err := db.execIndexOrderScan(plan, plan.indexOrder, env, meter)
		if err != nil {
			return emitter{}, err
		}
		return emitter{final: res.rows, mode: emitFinal}, nil
	}

	// Streaming external sort (spec/design/spill.md §5): a single-table, no-join, non-aggregate,
	// non-DISTINCT query with an ORDER BY the scan does NOT already satisfy (!plan.pkOrdered — caught
	// above) streams scan→filter→sorter, so the input is never materialized in the executor heap and
	// the sort spills sorted runs to disk under workMem (file-backed databases). DISTINCT/aggregate/
	// join take the eager path below, and an index bound does not stream (like the LIMIT
	// short-circuit). Results + cost are identical to the eager sort (the sort is unmetered —
	// cost.md §3; spill.md §6).
	if len(plan.order) > 0 && !plan.pkOrdered && len(plan.orderExprs) == 0 && len(plan.rels) == 1 && len(plan.joins) == 0 &&
		!plan.isAgg && !plan.hasWindow && !plan.distinct &&
		!plan.relBounds[0].needsEagerScan() &&
		plan.rels[0].srf == nil &&
		// A CTE reference takes the eager path (cte.md §5).
		plan.rels[0].cte == nil &&
		// A derived table takes the eager path (grammar.md §42).
		plan.rels[0].derived == nil {
		// The streaming sort yields its output LAZILY (streaming.md §4/§7) — execStreamingSort runs the
		// scan + sort + OFFSET skip and returns an emitSorted emitter over the `sorted` pull iterator;
		// the window's row_produced + projection is charged by the emitter drive.
		return db.execStreamingSort(plan, env, meter, params)
	}

	// Streaming two-table join (cost.md §3 "JOIN"): the planner set joinPkOrdered only for a two-table
	// INNER/CROSS join whose ORDER BY the OUTER relation's PK scan order satisfies, with a LIMIT. The
	// nested loop drives the outer in PK order so the output is already ordered — the sort is elided
	// and the loop short-circuits a top-N.
	if plan.joinPkOrdered {
		res, err := db.execStreamingJoin(plan, env, meter, params, outer, env.rng)
		if err != nil {
			return emitter{}, err
		}
		return emitter{final: res.rows, mode: emitFinal}, nil
	}

	// Windowed top-N (spec/design/window.md §5.2, cost.md §3): a plain window query whose LIMIT is
	// answerable from the first OFFSET+LIMIT PK-scan rows (a backward window over the PK-ordered scan)
	// scans only that prefix instead of the whole table — the window analog of the streaming LIMIT
	// short-circuit. Ineligible window queries fall through to the eager whole-table materialize below.
	if db.windowTopNEligible(plan) {
		return db.execWindowTopN(plan, env, meter, params)
	}

	// Materialize each relation once, in primary-key order (base tables drain a scanSource — the
	// page_read block + per-row storage_row_read accrue inside next(), cost.md §3). The nested loop
	// re-reads from these in-memory buffers, which are not stores and charge nothing. A CORRELATED
	// LATERAL relation (§44) depends on the left-hand row, so it cannot be materialized up front — a
	// placeholder (nil) holds its slot and the join loop re-materializes it per combined left row.
	// An INDEX-NESTED-LOOP relation (cost.md §3 "JOIN") likewise depends on the left-hand row (its
	// bound seeks per outer row), so it is not materialized up front either — a placeholder (nil)
	// holds its slot and the join loop re-materializes it per left row.
	materialized := make([][]storedRow, len(plan.rels))
	for ri, rel := range plan.rels {
		if rel.lateral || plan.relINLBounds[ri] != nil {
			continue
		}
		rows, err := db.materializeRel(plan, ri, params, outer, nil, env.rng, env.ctes, meter)
		if err != nil {
			return emitter{}, err
		}
		materialized[ri] = rows
	}

	// Left-deep nested-loop join. `running` holds the combined rows over the relations joined
	// so far (starting with the first table's rows). For each join, concatenate every running
	// row with every right-table row; CROSS keeps all pairs, INNER keeps a pair iff its ON
	// predicate is TRUE (three-valued — a NULL join key never matches). LEFT/FULL additionally
	// emit each unmatched left row NULL-extended over the right side; RIGHT/FULL emit each
	// unmatched right row NULL-extended over the left side. The NULL-extension appends evaluate
	// no ON (no operator_eval — spec/design/cost.md §3). Output order is deterministic: running
	// order (outer) then right key order (inner), each unmatched left row after its (empty)
	// match run, all unmatched right rows last in right key order (CLAUDE.md §10).
	// A FROM-less SELECT has no relations: seed `running` with ONE virtual zero-column row
	// instead of a table's rows (grammar.md §34). No scan ran, so no scan cost accrued.
	running := []storedRow{{}}
	if len(plan.rels) > 0 {
		running = materialized[0]
	}
	for k := range plan.joins {
		on := plan.joins[k].on
		emitLeft := plan.joins[k].kind == joinLeft || plan.joins[k].kind == joinFull
		emitRight := plan.joins[k].kind == joinRight || plan.joins[k].kind == joinFull
		// NULL-pad widths come from the PLAN, never a sampled row, so they are correct even when
		// `running`/`rightRows` is empty: the right table begins at flat offset rels[k+1].offset
		// (= the width of every running row) and is that many columns wide.
		leftPad := plan.rels[k+1].offset
		rightPad := plan.rels[k+1].colCount
		var next []storedRow
		// A CORRELATED LATERAL relation (§44): re-materialize it ONCE PER combined left-hand row, with
		// that row pushed onto the outer-row stack as the body's immediate outer (the correlated-
		// subquery mechanism). The plan guarantees INNER/CROSS/LEFT here (RIGHT/FULL to a correlated
		// lateral is 42P10), so there is no unmatched-right emission.
		if plan.rels[k+1].lateral {
			for _, left := range running {
				latOuter := make([]storedRow, len(outer)+1)
				copy(latOuter, outer)
				latOuter[len(outer)] = left
				rightRows, err := db.materializeRel(plan, k+1, params, latOuter, nil, env.rng, env.ctes, meter)
				if err != nil {
					return emitter{}, err
				}
				leftMatched := false
				for _, right := range rightRows {
					combined := make(storedRow, 0, len(left)+len(right))
					combined = append(combined, left...)
					combined = append(combined, right...)
					keep := true
					if on != nil {
						v, err := on.eval(combined, env, meter)
						if err != nil {
							return emitter{}, err
						}
						keep = v.IsTrue()
					}
					if keep {
						next = append(next, combined)
						leftMatched = true
					}
				}
				if emitLeft && !leftMatched {
					combined := make(storedRow, 0, len(left)+rightPad)
					combined = append(combined, left...)
					for i := 0; i < rightPad; i++ {
						combined = append(combined, NullValue())
					}
					next = append(next, combined)
				}
			}
			running = next
			continue
		}
		// An INDEX-NESTED-LOOP inner relation (cost.md §3 "JOIN"): re-materialize it ONCE PER combined
		// left-hand row, its scan bounded per outer row by the SIBLING columns of that row (a
		// per-outer-row seek instead of a full scan). Detection restricts this to the RIGHT/nullable
		// side of an INNER/CROSS/LEFT join, so there is never an unmatched-RIGHT emission (RIGHT/FULL
		// are excluded — a preserved side cannot be bounded per outer row). The whole ON/WHERE stays
		// applied (the ON here, the WHERE below), so rows are unchanged.
		if plan.relINLBounds[k+1] != nil {
			for _, left := range running {
				rightRows, err := db.materializeRel(plan, k+1, params, outer, left, env.rng, env.ctes, meter)
				if err != nil {
					return emitter{}, err
				}
				leftMatched := false
				for _, right := range rightRows {
					combined := make(storedRow, 0, len(left)+len(right))
					combined = append(combined, left...)
					combined = append(combined, right...)
					keep := true
					if on != nil {
						v, err := on.eval(combined, env, meter)
						if err != nil {
							return emitter{}, err
						}
						keep = v.IsTrue()
					}
					if keep {
						next = append(next, combined)
						leftMatched = true
					}
				}
				if emitLeft && !leftMatched {
					combined := make(storedRow, 0, len(left)+rightPad)
					combined = append(combined, left...)
					for i := 0; i < rightPad; i++ {
						combined = append(combined, NullValue())
					}
					next = append(next, combined)
				}
			}
			running = next
			continue
		}
		rightRows := materialized[k+1]
		rightMatched := make([]bool, len(rightRows))
		for _, left := range running {
			leftMatched := false
			for ri, right := range rightRows {
				combined := make(storedRow, 0, len(left)+len(right))
				combined = append(combined, left...)
				combined = append(combined, right...)
				keep := true
				if on != nil {
					v, err := on.eval(combined, env, meter)
					if err != nil {
						return emitter{}, err
					}
					keep = v.IsTrue()
				}
				if keep {
					next = append(next, combined)
					leftMatched = true
					rightMatched[ri] = true
				}
			}
			if emitLeft && !leftMatched {
				combined := make(storedRow, 0, len(left)+rightPad)
				combined = append(combined, left...)
				for i := 0; i < rightPad; i++ {
					combined = append(combined, NullValue())
				}
				next = append(next, combined)
			}
		}
		if emitRight {
			for ri, right := range rightRows {
				if !rightMatched[ri] {
					combined := make(storedRow, 0, leftPad+len(right))
					for i := 0; i < leftPad; i++ {
						combined = append(combined, NullValue())
					}
					combined = append(combined, right...)
					next = append(next, combined)
				}
			}
		}
		running = next
	}

	// WHERE over the combined rows. A WHERE arithmetic can trap (22003/22012); each surviving
	// combined row's filter accrues operator_eval.
	var rows []storedRow
	for _, row := range running {
		keep := true
		if plan.filter != nil {
			v, err := plan.filter.eval(row, env, meter)
			if err != nil {
				return emitter{}, err
			}
			keep = v.IsTrue()
		}
		if keep {
			rows = append(rows, row)
		}
	}

	// WINDOW stage (spec/design/window.md §5.2): a blocking operator over the post-WHERE rows,
	// running BEFORE the query ORDER BY / DISTINCT / LIMIT. Each window function's per-row result is
	// APPENDED to its row (so the projection reads result i at flat slot input_width+i); the rows
	// keep their scan order, and the query ORDER BY below re-sorts the extended rows. A window query
	// never enters the streaming fast-paths above. A GROUPED window query (§2) runs the window stage
	// over the GROUPED rows instead, inside the aggregate branch below — so it is gated to plain
	// (non-aggregate) windows here.
	if plan.hasWindow && !plan.isAgg {
		if err := applyWindowStage(rows, plan.windowSpecs, plan.windowKeys, env, meter); err != nil {
			return emitter{}, err
		}
	}

	// ORDER BY: stable sort applying each key left to right — the first non-equal key decides,
	// and a full tie keeps the scan order (SliceStable). Each key's NULL placement is decoupled
	// from its value-direction flip (spec/design/grammar.md §10). Aggregate queries sort their
	// GROUP rows in the aggregate branch below — not these pre-aggregation rows — so this is
	// gated to plain queries.
	if !plan.isAgg && len(plan.order) > 0 {
		// Materialize each general-expression ORDER BY key (grammar.md §10): evaluate it against the
		// post-WHERE (post-window) row and append the value, so its sort slot final_width+k reads the
		// appended column and the slot-based sort below is unchanged — the window-key precedent. The
		// evaluation is metered per node (cost.md §3); a no-op for a column/ordinal-only ORDER BY.
		if err := materializeOrderExprs(rows, plan.orderExprs, env, meter); err != nil {
			return emitter{}, err
		}
		if err := sortRows(rows, plan.order); err != nil {
			return emitter{}, err
		}
	}

	// LIMIT / OFFSET window bounds over a result of n rows. Clamp in the i64 domain
	// against the row count before indexing — never truncate a huge count (CLAUDE.md §8;
	// spec/design/grammar.md §9). The counts are already non-negative (parser).
	windowBounds := func(n int64) (int64, int64) {
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

	// Build the emitter. The two paths differ in pipeline order (spec/design/grammar.md §11): without
	// DISTINCT the window slices the sorted source rows and ONLY the windowed rows are projected (on
	// emission); with DISTINCT every (sorted) filtered row is projected — dedup must see them all —
	// duplicates drop by first occurrence, and the window then slices the DISTINCT rows.
	var em emitter
	if plan.isAgg {
		// Aggregate query — group + accumulate (aggregates.md §5). Bucket the post-WHERE rows by
		// their group-key values; the bucket key is the value-canonical distinctRowKey (it
		// collapses 1.5/1.50 and groups NULL with NULL), and the map is only an index — output
		// order comes from the insertion-ordered `groups`, never map iteration (no map-order leak
		// — CLAUDE.md §8/§10). Whole-table aggregation (no GROUP BY) is one pre-created empty-key
		// group, so it emits ONE row even over zero input; GROUP BY over an empty table creates no
		// groups -> zero rows. Each (row × aggregate) charges aggregate_accumulate; the operand's
		// own operator_evals accrue via eval; the bucketing/finalize is unmetered (cost.md §3).
		// Per group: its key values, one acc per aggregate, and one DISTINCT dedup set per
		// DISTINCT aggregate (nil for a plain aggregate — COUNT(DISTINCT x), aggregates.md §5).
		// The set keys on distinctRowKey, the same value-canonical form as the group-key
		// bucketing (so 1.5 == 1.50 and -0.0 == 0.0).
		type group struct {
			keys []Value
			accs []*acc
			seen []map[string]bool
		}
		newSeen := func() []map[string]bool {
			s := make([]map[string]bool, len(plan.aggSpecs))
			for i, spec := range plan.aggSpecs {
				if spec.distinct {
					s[i] = make(map[string]bool)
				}
			}
			return s
		}
		newAccs := func() []*acc {
			a := make([]*acc, len(plan.aggSpecs))
			for i, spec := range plan.aggSpecs {
				a[i] = newAccFromSpec(spec)
			}
			return a
		}
		// One grouping set per plan.groupSets (spec/design/aggregates.md §12): a plain GROUP BY /
		// whole-table aggregate has exactly one; ROLLUP / CUBE / GROUPING SETS have several. Each set is
		// bucketed independently over the SAME post-WHERE rows and its groups projected into the shared
		// synthetic row [master_columns…, aggregate_results…, GROUPING_results…]: a column not grouped
		// in this set is NULL, and each GROUPING() value comes from this set's mask. The scan
		// (storage_row_read) is upstream and counted once; aggregate_accumulate + operand evals accrue
		// per (set × row × passing aggregate). The per-set bucket index is never iterated — output order
		// comes from the insertion-ordered groups then the set order (no map-order leak — §8/§10).
		// Materialize the general-expression GROUP BY keys (aggregates.md §15): evaluate each per
		// post-WHERE row ONCE (charging its operator_evals, like an aggregate operand) and append the
		// value at flat slot inputWidth+k, so a master grouping-key index pointing there reads it. Done
		// before the (possibly multi-) grouping-set loop so each row is extended once and the values are
		// shared across sets. A plain column GROUP BY appends nothing.
		if len(plan.groupExprs) > 0 {
			for ri := range rows {
				if err := meter.Guard(); err != nil {
					return emitter{}, err
				}
				for _, ge := range plan.groupExprs {
					v, gerr := ge.eval(rows[ri], env, meter)
					if gerr != nil {
						return emitter{}, gerr
					}
					rows[ri] = append(rows[ri], v)
				}
			}
		}
		var groupRows []storedRow
		for gsi := range plan.groupSets {
			gset := &plan.groupSets[gsi]
			index := make(map[string]int)
			var groups []group
			// An empty grouping set (the () / whole-table grand total) is one pre-created group, so it
			// emits ONE row even over zero input; a non-empty set over empty input emits nothing.
			if len(gset.keyCols) == 0 {
				groups = append(groups, group{keys: nil, accs: newAccs(), seen: newSeen()})
				index[""] = 0
			}
			for _, row := range rows {
				if err := meter.Guard(); err != nil { // enforce the cost ceiling per folded row (CLAUDE.md §13)
					return emitter{}, err
				}
				keys := make([]Value, len(gset.keyCols))
				for i, gk := range gset.keyCols {
					keys[i] = row[gk]
				}
				k := distinctRowKey(keys)
				gi, ok := index[k]
				if !ok {
					gi = len(groups)
					index[k] = gi
					groups = append(groups, group{keys: keys, accs: newAccs(), seen: newSeen()})
				}
				for i, spec := range plan.aggSpecs {
					// FILTER (WHERE cond): a row for which the filter is not TRUE (FALSE or NULL)
					// contributes nothing to THIS aggregate — its operand is not evaluated and it is not
					// accumulated (aggregates.md §11). The filter's own operator_evals are charged (it is
					// evaluated per row, like the operand); aggregate_accumulate is charged only for a row
					// that passes. The pass/fold decision is deterministic (scan order is cross-core
					// identical), so the metered cost is identical across cores.
					if spec.filter != nil {
						fv, ferr := spec.filter.eval(row, env, meter)
						if ferr != nil {
							return emitter{}, ferr
						}
						if !fv.IsTrue() {
							continue
						}
					}
					meter.Charge(costs.AggregateAccumulate)
					// A hypothetical-set aggregate (rank/dense_rank/… — aggregates.md §19) buffers the
					// row's WITHIN GROUP key TUPLE (no NULL-skip — every row counts, sorted by NULLS
					// FIRST/LAST). The hypothetical row itself is evaluated per group at finalize. No
					// DISTINCT (rejected at resolve).
					if spec.hypo != nil {
						tuple := make([]Value, len(spec.hypo.keys))
						for ki, k := range spec.hypo.keys {
							kv, kerr := k.eval(row, env, meter)
							if kerr != nil {
								return emitter{}, kerr
							}
							tuple[ki] = kv
						}
						a := groups[gi].accs[i]
						a.hypoRows = append(a.hypoRows, tuple)
						continue
					}
					v := NullValue() // COUNT(*) ignores the value
					if spec.operand != nil {
						var verr error
						if v, verr = spec.operand.eval(row, env, meter); verr != nil {
							return emitter{}, verr
						}
					}
					// DISTINCT: skip a NULL (never folded by any aggregate) and any value already
					// folded into this group — the FIRST occurrence in scan order wins, so the set of
					// folded values (and the decimal_work fold charges) is order-deterministic and
					// cross-core identical.
					if seen := groups[gi].seen[i]; seen != nil {
						if v.IsNull() {
							continue
						}
						dk := distinctRowKey([]Value{v})
						if seen[dk] {
							continue
						}
						seen[dk] = true
					}
					if ferr := groups[gi].accs[i].fold(v, meter); ferr != nil {
						return emitter{}, ferr
					}
				}
			}
			// Build one synthetic row per group of this set: each master grouping column's value (NULL
			// where this set doesn't group it), then the aggregate results, then each GROUPING() value
			// (computed from this set's mask — spec/design/aggregates.md §12).
			for _, g := range groups {
				srow := make([]Value, 0, len(plan.groupKeys)+len(plan.aggSpecs)+len(plan.groupingSpecs))
				for _, src := range gset.slotSrc {
					if src < 0 {
						srow = append(srow, NullValue())
					} else {
						srow = append(srow, g.keys[src])
					}
				}
				for si, a := range g.accs {
					// An ordered-set aggregate's percentile fraction (the direct argument) is
					// evaluated PER GROUP here, against the synthetic row's grouping-key values
					// (aggregates.md §13/§17) — so it may reference grouping columns. Unmetered
					// (the finalize step, like the sort), via a scratch meter. mode has none.
					if fe := plan.aggSpecs[si].osaFrac; fe != nil {
						fv, ferr := fe.eval(srow, env, &costMeter{})
						if ferr != nil {
							return emitter{}, ferr
						}
						a.osaFrac = &fv
					}
					// A hypothetical-set aggregate is finalized INLINE here (not via acc.finalize)
					// because it needs the spec's per-key sort specs: evaluate the hypothetical row's
					// direct args per group (against the synthetic row, like a fraction — unmetered
					// scratch meter), then count its rank among the buffered key tuples (aggregates.md §19).
					if hp := plan.aggSpecs[si].hypo; hp != nil {
						hyp := make([]Value, len(hp.args))
						for ai, arg := range hp.args {
							av, aerr := arg.eval(srow, env, &costMeter{})
							if aerr != nil {
								return emitter{}, aerr
							}
							hyp[ai] = av
						}
						v, ferr := finalizeHypothetical(a.plan, a.hypoRows, hyp, hp.sorts)
						if ferr != nil {
							return emitter{}, ferr
						}
						srow = append(srow, v)
						continue
					}
					v, ferr := a.finalize()
					if ferr != nil {
						return emitter{}, ferr
					}
					srow = append(srow, v)
				}
				for _, positions := range plan.groupingSpecs {
					srow = append(srow, IntValue(groupingValue(positions, gset.mask)))
				}
				groupRows = append(groupRows, srow)
			}
		}
		// HAVING: filter the grouped rows (after aggregation, before ORDER BY). The predicate is
		// evaluated against each group's synthetic row (charging its operator_evals per group);
		// only a TRUE result keeps the group. A dropped group charges no row_produced (§8).
		if plan.having != nil {
			kept := groupRows[:0:0]
			for _, srow := range groupRows {
				v, herr := plan.having.eval(srow, env, meter)
				if herr != nil {
					return emitter{}, herr
				}
				if v.IsTrue() {
					kept = append(kept, srow)
				}
			}
			groupRows = kept
		}
		// WINDOW stage over the grouped rows (spec/design/window.md §2): runs AFTER GROUP BY/HAVING
		// and BEFORE the query ORDER BY. It appends each window result to the grouped row
		// [group_keys…, agg_results…], so the projection reads window result w at slot
		// len(groupKeys)+len(aggSpecs)+w (the rebased slot — §5.1). The group-key slots the ORDER BY
		// below sorts on are unchanged (they precede the appended results).
		if plan.hasWindow {
			if err := applyWindowStage(groupRows, plan.windowSpecs, plan.windowKeys, env, meter); err != nil {
				return emitter{}, err
			}
		}
		// ORDER BY over the grouped output (a column/ordinal key is a synthetic group-key slot; an
		// expression key is materialized against the grouped row and appended — grammar.md §10).
		if len(plan.order) > 0 {
			if err := materializeOrderExprs(groupRows, plan.orderExprs, env, meter); err != nil {
				return emitter{}, err
			}
			if err := sortRows(groupRows, plan.order); err != nil {
				return emitter{}, err
			}
		}
		if plan.distinct {
			// SELECT DISTINCT: project EVERY grouped row (charging its projection operator_evals,
			// the §3 asymmetry — like the non-aggregate DISTINCT path below), dedup by equality
			// keeping the first occurrence in the (already ORDER-BY-sorted) order, then LIMIT/OFFSET.
			// `seen` is membership-only; output order comes from the deterministic group iteration /
			// sort, never map iteration (no map-order leak — CLAUDE.md §8/§10).
			seen := make(map[string]bool)
			var distinctRows [][]Value
			for _, srow := range groupRows {
				projected := make([]Value, len(plan.projections))
				for i, p := range plan.projections {
					v, perr := p.eval(srow, env, meter)
					if perr != nil {
						return emitter{}, perr
					}
					projected[i] = v
				}
				if key := distinctRowKey(projected); !seen[key] {
					seen[key] = true
					distinctRows = append(distinctRows, projected)
				}
			}
			// The dedup already projected every grouped row (the §3 asymmetry, charged above), so
			// emission is Identity — window + charge row_produced, deferred to the drive (streaming.md §4).
			start, end := windowBounds(int64(len(distinctRows)))
			em = emitter{final: distinctRows, start: start, end: end, mode: emitIdentity}
		} else {
			// Window then project on emission; only an emitted row charges row_produced + projection
			// cost. Deferred to the drive (streaming.md §4).
			start, end := windowBounds(int64(len(groupRows)))
			em = emitter{src: groupRows, start: start, end: end, mode: emitProject}
		}
	} else if plan.distinct {
		// Project every filtered row (charging projection cost per row, the §3 asymmetry),
		// keeping first occurrences. `seen` is membership-only: output order comes from the
		// deterministic source iteration, never from map iteration (no map-order leak —
		// CLAUDE.md §8/§10).
		seen := make(map[string]bool)
		var distinctRows [][]Value
		for _, row := range rows {
			projected := make([]Value, len(plan.projections))
			for i, p := range plan.projections {
				v, err := p.eval(row, env, meter)
				if err != nil {
					return emitter{}, err
				}
				projected[i] = v
			}
			if key := distinctRowKey(projected); !seen[key] {
				seen[key] = true
				distinctRows = append(distinctRows, projected)
			}
		}
		// LIMIT / OFFSET applies to the DISTINCT rows; only the emitted rows charge RowProduced
		// (spec/design/cost.md §3). The rows were already projected for their dedup key (the §3
		// asymmetry, charged above), so emission is Identity — deferred to the drive (streaming.md §4).
		start, end := windowBounds(int64(len(distinctRows)))
		em = emitter{final: distinctRows, start: start, end: end, mode: emitIdentity}
	} else {
		// Window the sorted rows BEFORE projection, so rows skipped by OFFSET or excluded by LIMIT
		// accrue no row_produced/projection cost (they were still scanned + filtered above). Producing
		// a row, and each projection-list evaluation, accrue cost on emission — deferred to the drive
		// (streaming.md §4). (ORDER BY's sort comparisons are not metered — spec/design/cost.md §3.)
		start, end := windowBounds(int64(len(rows)))
		em = emitter{src: rows, start: start, end: end, mode: emitProject}
	}
	return em, nil
}

// ---- Uncorrelated subquery folding (spec/design/grammar.md §26) ----------------------
//
// After the whole statement tree is planned + the parameters bound, this bottom-up pass walks
// every reSubquery node in the plan tree: it first folds within the node's own sub-plan, then —
// if the subquery references NO enclosing scope (a global constant, PG's "initplan") — executes
// it ONCE and replaces it with a constant (scalar -> its value; EXISTS -> a boolean; IN -> an
// reInValues over the result column), accruing the subquery's cost once (preserving the committed
// once-only cost — cost.md §3). A CORRELATED subquery is left in place; the evaluator re-executes
// it per outer row. So after this pass the only surviving reSubquery nodes are correlated.

func (db *engine) foldUncorrelatedInPlan(plan *queryPlan, bound []Value, ctes cteCtx, cost *int64) error {
	if plan.sel != nil {
		return db.foldUncorrelatedInSelect(plan.sel, bound, ctes, cost)
	}
	if plan.values != nil {
		// A VALUES-body value may itself hold an (uncorrelated) scalar subquery to fold once before
		// the rows are produced (grammar.md §42; the §26 fold).
		for r := range plan.values.rows {
			for c := range plan.values.rows[r] {
				if err := db.foldUncorrelatedInRExpr(plan.values.rows[r][c], bound, ctes, cost); err != nil {
					return err
				}
			}
		}
		return nil
	}
	if plan.with != nil {
		// A nested WITH body is not folded here against the enclosing ctes — its inner subqueries
		// reference the nested CTEs (a different scope, materialized only when the node runs), so
		// they are left to the evaluator, exactly like a derived table's body (spec/design/cte.md
		// §7). The whole nested-WITH subquery is itself folded by the caller if uncorrelated
		// (executed once via execWithPlan).
		return nil
	}
	if err := db.foldUncorrelatedInPlan(&plan.setop.lhs, bound, ctes, cost); err != nil {
		return err
	}
	return db.foldUncorrelatedInPlan(&plan.setop.rhs, bound, ctes, cost)
}

func (db *engine) foldUncorrelatedInSelect(sp *selectPlan, bound []Value, ctes cteCtx, cost *int64) error {
	for k := range sp.joins {
		if sp.joins[k].on != nil {
			if err := db.foldUncorrelatedInRExpr(sp.joins[k].on, bound, ctes, cost); err != nil {
				return err
			}
		}
	}
	if sp.filter != nil {
		if err := db.foldUncorrelatedInRExpr(sp.filter, bound, ctes, cost); err != nil {
			return err
		}
	}
	if sp.having != nil {
		if err := db.foldUncorrelatedInRExpr(sp.having, bound, ctes, cost); err != nil {
			return err
		}
	}
	for i := range sp.aggSpecs {
		if sp.aggSpecs[i].operand != nil {
			if err := db.foldUncorrelatedInRExpr(sp.aggSpecs[i].operand, bound, ctes, cost); err != nil {
				return err
			}
		}
	}
	for _, p := range sp.projections {
		if err := db.foldUncorrelatedInRExpr(p, bound, ctes, cost); err != nil {
			return err
		}
	}
	// A set-returning relation's arguments may themselves contain an (uncorrelated) subquery to
	// fold once before the generator runs (functions.md §10).
	for i := range sp.rels {
		if sp.rels[i].srf != nil {
			for _, a := range sp.rels[i].srf.args {
				if err := db.foldUncorrelatedInRExpr(a, bound, ctes, cost); err != nil {
					return err
				}
			}
		}
	}
	return nil
}

// foldUncorrelatedInRExpr folds this node if it is an uncorrelated reSubquery, else recurses into
// its children. A reSubquery is mutated IN PLACE (*e = ...) so every pointer to it sees the fold.
func (db *engine) foldUncorrelatedInRExpr(e *rExpr, bound []Value, ctes cteCtx, cost *int64) error {
	if e.kind == reSubquery {
		// Bottom-up: fold within this subquery's own sub-plan (and its IN lhs) first, so a
		// globally-uncorrelated subquery nested inside it is already a constant before we run it.
		if e.lhs != nil {
			if err := db.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost); err != nil {
				return err
			}
		}
		if err := db.foldUncorrelatedInPlan(e.subPlan, bound, ctes, cost); err != nil {
			return err
		}
		if queryPlanReferencesOuter(e.subPlan, 0) {
			return nil // correlated — re-executed per outer row at eval
		}
		// Uncorrelated: execute ONCE and fold to a constant / reInValues.
		r, err := db.execQueryPlan(e.subPlan, nil, bound, ctes)
		if err != nil {
			return err
		}
		*cost += r.cost
		switch e.subKind {
		case sqScalar:
			if len(r.rows) > 1 {
				return newError(CardinalityViolation, "more than one row returned by a subquery used as an expression")
			}
			val := NullValue()
			if len(r.rows) == 1 {
				val = r.rows[0][0]
			}
			*e = *valueToRExpr(val)
		case sqExists:
			*e = rExpr{kind: reConstBool, cBool: (len(r.rows) > 0) != e.negated}
		case sqQuantified:
			// An uncorrelated quantified subquery folds to a constant-array reQuantified
			// (array-functions.md §11.6): its single column becomes a 1-D array and the node reuses
			// the array form's 3VL fold — no per-row re-execution.
			elems := make([]Value, len(r.rows))
			for i, row := range r.rows {
				elems[i] = row[0]
			}
			arr := &rExpr{kind: reConstArray, cArray: oneDimArray(elems)}
			*e = rExpr{kind: reQuantified, op: e.op, quantAll: e.quantAll, lhs: e.lhs, rhs: arr}
		default: // sqIn
			list := make([]Value, len(r.rows))
			for i, row := range r.rows {
				list[i] = row[0]
			}
			*e = rExpr{kind: reInValues, lhs: e.lhs, list: list, negated: e.negated}
		}
		return nil
	}
	// Recurse into the children of every other node (a subquery may nest anywhere). The fields
	// are only set for the relevant node kinds, so this is exhaustive without a per-kind switch.
	if e.operand != nil {
		if err := db.foldUncorrelatedInRExpr(e.operand, bound, ctes, cost); err != nil {
			return err
		}
	}
	if e.lhs != nil {
		if err := db.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost); err != nil {
			return err
		}
	}
	if e.rhs != nil {
		if err := db.foldUncorrelatedInRExpr(e.rhs, bound, ctes, cost); err != nil {
			return err
		}
	}
	for _, arm := range e.caseArms {
		if err := db.foldUncorrelatedInRExpr(arm.cond, bound, ctes, cost); err != nil {
			return err
		}
		if err := db.foldUncorrelatedInRExpr(arm.result, bound, ctes, cost); err != nil {
			return err
		}
	}
	if e.caseEls != nil {
		if err := db.foldUncorrelatedInRExpr(e.caseEls, bound, ctes, cost); err != nil {
			return err
		}
	}
	for _, a := range e.sargs {
		if err := db.foldUncorrelatedInRExpr(a, bound, ctes, cost); err != nil {
			return err
		}
	}
	return nil
}

// queryPlanReferencesOuter reports whether a plan references any scope STRICTLY OUTSIDE itself —
// i.e. it is correlated (spec/design/grammar.md §26). depth is how many nested-subquery frames we
// have descended INTO this plan (0 = its own clauses); an reOuterColumn at level points above iff
// level > depth. The fold pass calls it with depth 0 on a subquery's sub-plan to fold (uncorrelated)
// or leave (correlated) it.
func queryPlanReferencesOuter(plan *queryPlan, depth int) bool {
	if plan.sel != nil {
		return selectPlanReferencesOuter(plan.sel, depth)
	}
	if plan.values != nil {
		// A VALUES body is planned parent=nil, so its values hold no outer reference of their own; a
		// folded-in subquery, however, may correlate to the target scope.
		for r := range plan.values.rows {
			for c := range plan.values.rows[r] {
				if rexprReferencesOuter(plan.values.rows[r][c], depth) {
					return true
				}
			}
		}
		return false
	}
	if plan.with != nil {
		// A nested WITH adds no correlation frame: its body is at the same depth, and the CTE bodies
		// are planned parent=nil (no outer reference), so only the body can correlate (cte.md §7).
		return queryPlanReferencesOuter(&plan.with.body, depth)
	}
	return queryPlanReferencesOuter(&plan.setop.lhs, depth) || queryPlanReferencesOuter(&plan.setop.rhs, depth)
}

func selectPlanReferencesOuter(sp *selectPlan, depth int) bool {
	for k := range sp.joins {
		if sp.joins[k].on != nil && rexprReferencesOuter(sp.joins[k].on, depth) {
			return true
		}
	}
	if sp.filter != nil && rexprReferencesOuter(sp.filter, depth) {
		return true
	}
	if sp.having != nil && rexprReferencesOuter(sp.having, depth) {
		return true
	}
	for i := range sp.aggSpecs {
		if sp.aggSpecs[i].operand != nil && rexprReferencesOuter(sp.aggSpecs[i].operand, depth) {
			return true
		}
	}
	for _, p := range sp.projections {
		if rexprReferencesOuter(p, depth) {
			return true
		}
	}
	// A materialized ORDER BY expression may itself carry a correlated reference (query.order_by_correlated):
	// a subquery whose ONLY outer reference is in its ORDER BY is still correlated and must re-execute per
	// outer row (else its OuterColumn reads an empty outer-row environment).
	for _, oe := range sp.orderExprs {
		if rexprReferencesOuter(oe, depth) {
			return true
		}
	}
	// A set-returning relation's arguments may carry a correlated reference (an implicitly-lateral
	// SRF arg sees params / outer / an earlier sibling — functions.md §10, grammar.md §44), making
	// the enclosing query correlated.
	for i := range sp.rels {
		if sp.rels[i].srf != nil {
			for _, a := range sp.rels[i].srf.args {
				if rexprReferencesOuter(a, depth) {
					return true
				}
			}
		}
		// A LATERAL derived table's body is one frame deeper; a reference in it back into this
		// query's outer counts here so the enclosing item is correctly flagged correlated (§44).
		if sp.rels[i].derived != nil && queryPlanReferencesOuter(sp.rels[i].derived, depth+1) {
			return true
		}
	}
	return false
}

func rexprReferencesOuter(e *rExpr, depth int) bool {
	switch e.kind {
	case reOuterColumn:
		return e.level > depth
	case reSubquery:
		// A nested subquery's own clauses are one frame deeper; its IN lhs is at this frame.
		if e.lhs != nil && rexprReferencesOuter(e.lhs, depth) {
			return true
		}
		return queryPlanReferencesOuter(e.subPlan, depth+1)
	case reInValues:
		return rexprReferencesOuter(e.lhs, depth)
	}
	if e.operand != nil && rexprReferencesOuter(e.operand, depth) {
		return true
	}
	if e.lhs != nil && rexprReferencesOuter(e.lhs, depth) {
		return true
	}
	if e.rhs != nil && rexprReferencesOuter(e.rhs, depth) {
		return true
	}
	for _, arm := range e.caseArms {
		if rexprReferencesOuter(arm.cond, depth) || rexprReferencesOuter(arm.result, depth) {
			return true
		}
	}
	if e.caseEls != nil && rexprReferencesOuter(e.caseEls, depth) {
		return true
	}
	for _, a := range e.sargs {
		if rexprReferencesOuter(a, depth) {
			return true
		}
	}
	return false
}

// collectTouched marks the combined-row columns an expression STATICALLY references — the
// touched set (cost.md §3 "The touched set"; large-values.md §14). Depth bookkeeping mirrors
// rexprReferencesOuter: walking the target plan's own clauses is depth 0 (a column touches);
// inside a nested subquery a column indexes the subquery's own row (ignored) and an outer
// column with level == depth is a correlated reference back into the target scope (touches).
// Purely syntactic — a never-taken CASE branch still touches — so the set is deterministic and
// cross-core identical (a §8 contract).
func collectTouched(e *rExpr, depth int, touched []bool) {
	if e == nil {
		return
	}
	switch e.kind {
	case reColumn:
		// A reColumn index beyond the real columns is a SYNTHETIC slot (an aggregate or window
		// result, spec/design/window.md §5.1), not a table column — it touches no stored data, so
		// the bound check skips it rather than panicking.
		if depth == 0 && e.index < len(touched) {
			touched[e.index] = true
		}
		return
	case reOuterColumn:
		// A correlated reference into the scope we are collecting for (its frame is `depth` levels up).
		// The index is a slot in that target scope's combined row; bounds-checked like the reColumn case
		// (a synthetic slot beyond the real columns touches no stored data). Callers collect at the depth
		// matching the reference's level — a correlated subquery at its nesting depth, a LATERAL SRF arg
		// at depth 1 (its sibling frame — functions.md §10).
		if e.level == depth && depth > 0 && e.index < len(touched) {
			touched[e.index] = true
		}
		return
	case reSubquery:
		collectTouched(e.lhs, depth, touched)
		collectTouchedPlan(e.subPlan, depth+1, touched)
		return
	case reInValues:
		collectTouched(e.lhs, depth, touched)
		return
	}
	collectTouched(e.operand, depth, touched)
	collectTouched(e.lhs, depth, touched)
	collectTouched(e.rhs, depth, touched)
	for _, arm := range e.caseArms {
		collectTouched(arm.cond, depth, touched)
		collectTouched(arm.result, depth, touched)
	}
	collectTouched(e.caseEls, depth, touched)
	for _, a := range e.sargs {
		collectTouched(a, depth, touched)
	}
}

// windowResultBase is the placeholder base a window query's window results carry until
// rebaseWindowResults rewrites them to inputWidth+len(windowKeys)+w (spec/design/window.md §5.1).
// Far above any real column/synthetic-slot count, and below 2^31 so it is valid on a 32-bit usize
// (the Rust wasm32 build) as well as f64-exact in the TS core. Kept identical across the three cores.
const windowResultBase = 1 << 28

// windowKeyBase is the placeholder base a materialized window-key expression (a non-column PARTITION
// BY / ORDER BY key — `PARTITION BY a + b`) carries until the rebase pass rewrites it to its real
// synthetic slot inputWidth+k (spec/design/window.md §5.1). Disjoint from windowResultBase's range,
// below 2^31 (32-bit-usize / wasm32 safe). A bare-column key is NOT materialized — it keeps its real row slot.
const windowKeyBase = 1 << 29

// groupingGsBase is the placeholder base a GROUPING(...) call carries until the rebase pass rewrites
// it to its real trailing synthetic slot len(groupKeys)+len(aggSpecs)+g (the GROUPING results follow
// the master columns + aggregate results — spec/design/aggregates.md §12). Disjoint from the window
// bases, below 2^31 (32-bit-usize / wasm32 safe). GROUPING is mutually exclusive with window functions, so its
// placeholders never coexist with the window ones in a projection.
const groupingGsBase = 1 << 30

// orderExprBase is the placeholder base a materialized ORDER BY EXPRESSION key's sort slot carries
// until it is rebased to its real trailing slot final_row_width+k (the materialized order values are
// appended after the input / window / grouped columns — grammar.md §10). Used only in the orderSlot
// idx field (a different namespace from the rExpr column bases above), but kept disjoint and below
// 2^31 (32-bit-usize / wasm32 safe) for the same reasons. A column / ordinal key keeps its real slot.
const orderExprBase = 1 << 27

// maxGroupingSets bounds a GROUP BY's total expansion (CUBE of n columns alone is 2^n). Beyond this
// the statement is aborted 54001 (statement_too_complex) — jed's structural-complexity gate (a
// deliberate divergence from PostgreSQL's per-construct "CUBE is limited to 12 elements" / 54011;
// jed bounds the total expansion instead). spec/design/aggregates.md §12.
const maxGroupingSets = 4096

// groupSetPlan is one resolved grouping set of a GROUP BY (spec/design/aggregates.md §12). A plain
// GROUP BY has exactly one; ROLLUP/CUBE/GROUPING SETS produce several. Each is bucketed independently
// over the post-WHERE rows and its groups projected into the shared synthetic row, whose first
// len(groupKeys) slots are the master grouping columns (the ordered union of all sets' columns).
type groupSetPlan struct {
	// keyCols are the flat input-row indices this set buckets on (its key, in key order). Empty = one
	// grand-total group (always emits one row, even over an empty input — the () / whole-table case).
	keyCols []int
	// slotSrc is per master grouping-column slot (length len(groupKeys)): >= 0 if this set includes
	// that column (its synthetic value is the bucket key's slotSrc[p]-th component), else -1, meaning
	// the column is not grouped in this set and its synthetic value is NULL.
	slotSrc []int
	// mask is the GROUPING() bitmask for rows from this set: bit p is set iff master slot p is NOT in
	// this set.
	mask int64
}

// groupItemSetCount is the number of grouping sets a single GROUP BY term expands to, saturating well
// below the int max so a huge CUBE cannot overflow the product before the maxGroupingSets check.
func groupItemSetCount(item *groupItem) int {
	switch item.Kind {
	case groupSet:
		return 1
	case groupRollup:
		return len(item.Groups) + 1
	case groupCube:
		if len(item.Groups) >= 20 {
			return maxGroupingSets + 1
		}
		return 1 << len(item.Groups)
	case groupGroupingSets:
		total := 0
		for i := range item.Elems {
			total += groupItemSetCount(&item.Elems[i])
			if total > maxGroupingSets {
				return maxGroupingSets + 1
			}
		}
		return total
	}
	return 1
}

// expandGroupItem expands a single GROUP BY term into its grouping sets, each a list of column Exprs
// (ROLLUP/CUBE/GROUPING SETS and nesting — spec/design/aggregates.md §12). The per-set column order
// is textual; the set order is deterministic and identical across cores (tests compare with rowsort).
func expandGroupItem(item *groupItem) [][]*exprNode {
	switch item.Kind {
	case groupSet:
		set := make([]*exprNode, len(item.Cols))
		for i := range item.Cols {
			set[i] = &item.Cols[i]
		}
		return [][]*exprNode{set}
	case groupRollup:
		// The prefixes longest-first down to the empty set — n+1 sets.
		out := make([][]*exprNode, 0, len(item.Groups)+1)
		for k := len(item.Groups); k >= 0; k-- {
			var set []*exprNode
			for i := 0; i < k; i++ {
				for j := range item.Groups[i] {
					set = append(set, &item.Groups[i][j])
				}
			}
			out = append(out, set)
		}
		return out
	case groupCube:
		// Every subset of the column groups — 2^n sets (bit i = include group i).
		n := len(item.Groups)
		out := make([][]*exprNode, 0, 1<<n)
		for mask := 0; mask < (1 << n); mask++ {
			var set []*exprNode
			for i := 0; i < n; i++ {
				if mask&(1<<i) != 0 {
					for j := range item.Groups[i] {
						set = append(set, &item.Groups[i][j])
					}
				}
			}
			out = append(out, set)
		}
		return out
	case groupGroupingSets:
		var out [][]*exprNode
		for i := range item.Elems {
			out = append(out, expandGroupItem(&item.Elems[i])...)
		}
		return out
	}
	return nil
}

// expandGroupBy expands a whole GROUP BY clause into its grouping sets: the cross-product of the
// top-level terms' expansions. An empty clause yields one empty set (the whole-table grand total).
// Aborts 54001 if the expansion exceeds maxGroupingSets (spec/design/aggregates.md §12).
func expandGroupBy(items []groupItem) ([][]*exprNode, error) {
	total := 1
	for i := range items {
		total *= groupItemSetCount(&items[i])
		if total > maxGroupingSets {
			return nil, newError(StatementTooComplex, fmt.Sprintf("too many grouping sets (the limit is %d)", maxGroupingSets))
		}
	}
	acc := [][]*exprNode{{}}
	for i := range items {
		exp := expandGroupItem(&items[i])
		next := make([][]*exprNode, 0, len(acc)*len(exp))
		for _, a := range acc {
			for _, s := range exp {
				combined := make([]*exprNode, 0, len(a)+len(s))
				combined = append(combined, a...)
				combined = append(combined, s...)
				next = append(next, combined)
			}
		}
		acc = next
	}
	return acc, nil
}

// groupKeyExpr records a general-expression GROUP BY key (`GROUP BY a + b`, aggregates.md §15): its
// canonical AST (so a matching projection / HAVING / ORDER BY expression resolves to its synthetic
// slot) and its resolved type.
type groupKeyExpr struct {
	canon exprNode
	ty    resolvedType
}

// groupKeyResolved is the resolution of one GROUP BY grouping term (aggregates.md §15): either an
// input COLUMN at a flat row index (isColumn, index), or a general EXPRESSION to materialize
// (node + ty + canonical AST). Mirrors Rust's GroupKeyResolved enum.
type groupKeyResolved struct {
	isColumn bool
	index    int    // valid when isColumn
	node     *rExpr // valid when !isColumn — the materialized expression
	ty       resolvedType
	canon    exprNode // valid when !isColumn — the canonical AST kept for projection matching
}

// resolveGroupTerm resolves one GROUP BY grouping term to a column or a materialized expression
// (aggregates.md §15). Classifies the term: a bare integer literal is a select-list ORDINAL (1-based;
// out of range 42P10) whose target select item is then resolved as a term; otherwise it is a column
// / alias / general expression (resolveGroupNamed).
func resolveGroupTerm(s *scope, term exprNode, items selectItems, params *paramTypes) (groupKeyResolved, error) {
	// Only a *bare* integer literal is an ordinal — `GROUP BY 1`; `GROUP BY 1 + 1` is a constant
	// expression (PG). The parser folds a unary minus into the value, so a negative is just out of
	// range. The select list fixes the position count: `*` expands to the scope width.
	if term.Kind == exprLiteral && term.Literal != nil && term.Literal.Kind == literalInt {
		n := term.Literal.Int
		var ncols int64
		if items.All {
			ncols = int64(s.width())
		} else {
			ncols = int64(len(items.Items))
		}
		if n < 1 || n > ncols {
			return groupKeyResolved{}, newError(InvalidColumnReference,
				fmt.Sprintf("GROUP BY position %d is not in select list", n))
		}
		pos := int(n - 1)
		if items.All {
			// `SELECT *` — the ordinal names the column at that scope position directly.
			return groupKeyResolved{isColumn: true, index: pos}, nil
		}
		return resolveGroupExpr(s, items.Items[pos].Expr, params)
	}
	return resolveGroupNamed(s, term, items, params)
}

// resolveGroupNamed resolves a non-ordinal grouping term: a bare/qualified column, an output alias,
// or a general expression (aggregates.md §15). A bare name resolves an INPUT column FIRST, then —
// only if there is no such column — an output alias (PG's rule, the opposite of ORDER BY's
// output-first rule).
func resolveGroupNamed(s *scope, term exprNode, items selectItems, params *paramTypes) (groupKeyResolved, error) {
	switch term.Kind {
	case exprColumn:
		r, err := s.resolveBare(term.Column)
		if err != nil {
			// No input column of this name: try an output alias (`SELECT a+b AS s … GROUP BY s`). If
			// none matches either, propagate the original 42703.
			if se, ok := err.(*EngineError); ok && se.State == UndefinedColumn {
				aexpr, aerr := orderAliasMatch(items, term.Column, s)
				if aerr != nil {
					return groupKeyResolved{}, aerr
				}
				if aexpr != nil {
					return resolveGroupExpr(s, *aexpr, params)
				}
			}
			return groupKeyResolved{}, err
		}
		if r.level != 0 {
			return groupKeyResolved{}, newError(FeatureNotSupported, "GROUP BY may not reference an outer query column")
		}
		return groupKeyResolved{isColumn: true, index: r.index}, nil
	case exprQualifiedColumn:
		r, err := s.resolveQualified(term.Qualifier, term.Column)
		if err != nil {
			return groupKeyResolved{}, err
		}
		if r.level != 0 {
			return groupKeyResolved{}, newError(FeatureNotSupported, "GROUP BY may not reference an outer query column")
		}
		return groupKeyResolved{isColumn: true, index: r.index}, nil
	default:
		return resolveGroupExpr(s, term, params)
	}
}

// resolveGroupExpr resolves a grouping expression (the target of an ordinal/alias, or a general
// `GROUP BY a+b`). A plain column expression stays a COLUMN key (so the projection's bare-column path
// matches it); anything else is MATERIALIZED — resolved against the input row with aggregates
// forbidden (an aggregate in GROUP BY is 42803), its canonical AST kept for projection matching
// (aggregates.md §15).
func resolveGroupExpr(s *scope, e exprNode, params *paramTypes) (groupKeyResolved, error) {
	switch e.Kind {
	case exprColumn:
		if r, err := s.resolveBare(e.Column); err == nil && r.level == 0 {
			return groupKeyResolved{isColumn: true, index: r.index}, nil
		}
	case exprQualifiedColumn:
		if r, err := s.resolveQualified(e.Qualifier, e.Column); err == nil && r.level == 0 {
			return groupKeyResolved{isColumn: true, index: r.index}, nil
		}
	}
	sub := &aggCtx{collecting: false}
	node, ty, err := resolve(s, e, nil, sub, params)
	if err != nil {
		return groupKeyResolved{}, err
	}
	return groupKeyResolved{node: node, ty: ty, canon: e}, nil
}

// matchGroupExpr reports whether e structurally matches a general-expression GROUP BY key in this
// aggregate context; if so it returns that group's synthetic key slot (its master position) and
// resolved type (aggregates.md §15). Only fires in a collecting context with groupKeyExprs; an
// aggregate operand / FILTER resolves under Forbidden (no groupKeyExprs), so a grouping expression
// there is correctly NOT remapped (it is a per-row value, not the group key).
func matchGroupExpr(ag *aggCtx, e exprNode) (int, resolvedType, bool) {
	if ag == nil {
		return 0, resolvedType{}, false
	}
	for p, gk := range ag.groupKeyExprs {
		if gk != nil && exprEqual(gk.canon, e) {
			return p, gk.ty, true
		}
	}
	return 0, resolvedType{}, false
}

// groupingValue computes a GROUPING(args) result for a group from the grouping set whose mask is
// given: bit (k-1-j) of the result is bit positions[j] of mask (spec/design/aggregates.md §12).
func groupingValue(positions []int, mask int64) int64 {
	k := len(positions)
	var r int64
	for j, p := range positions {
		bit := (mask >> uint(p)) & 1
		r |= bit << uint(k-1-j)
	}
	return r
}

// rebasePlaceholderCols rewrites placeholder column slots in [from, 2·from) — a window-result
// (windowResultBase+w), a materialized window-key (windowKeyBase+k), or a GROUPING() (groupingGsBase+g)
// placeholder — to their real synthetic slot target+(slot-from), once the grouped/windowed row layout
// is final (spec/design/window.md §5.1, aggregates.md §12/§21). During resolution a window result of
// index w is assigned the placeholder windowResultBase+w, because its real slot
// len(groupKeys)+len(aggSpecs)+w is unknown until every aggregate (including any nested in a later
// window argument or HAVING) has been collected. Each placeholder base is 2× the previous (1<<28,
// 1<<29, 1<<30) and a base's placeholder count is far below that gap, so bounding the rewrite to
// [from, 2·from) keeps the bases isolated — a window-result rebase no longer clobbers a GROUPING()
// placeholder (the two now COEXIST in a GROUPING SETS + window query — aggregates.md §21). It descends
// into a subquery's lhs (current row space) but NOT its plan (those columns index the subquery's own
// rows; a nested grouped+window plan was already rebased when it was built).
func rebasePlaceholderCols(e *rExpr, from, target int) {
	if e == nil {
		return
	}
	switch e.kind {
	case reColumn:
		if e.index >= from && e.index < from*2 {
			e.index = target + (e.index - from)
		}
		return
	case reOuterColumn:
		return
	case reSubquery:
		rebasePlaceholderCols(e.lhs, from, target) // current row space only; not subPlan
		return
	case reInValues:
		rebasePlaceholderCols(e.lhs, from, target)
		return
	}
	rebasePlaceholderCols(e.operand, from, target)
	rebasePlaceholderCols(e.lhs, from, target)
	rebasePlaceholderCols(e.rhs, from, target)
	for _, arm := range e.caseArms {
		rebasePlaceholderCols(arm.cond, from, target)
		rebasePlaceholderCols(arm.result, from, target)
	}
	rebasePlaceholderCols(e.caseEls, from, target)
	for _, a := range e.sargs {
		rebasePlaceholderCols(a, from, target)
	}
	for _, sub := range e.subs {
		rebasePlaceholderCols(sub.index, from, target)
		rebasePlaceholderCols(sub.lower, from, target)
		rebasePlaceholderCols(sub.upper, from, target)
	}
}

// collectTouchedPlan walks a nested plan's expression surfaces for outer references back into
// the target scope — the same five surfaces selectPlanReferencesOuter checks (slot lists like
// group keys / ORDER BY index the nested plan's own rows and can never reach outward).
func collectTouchedPlan(plan *queryPlan, depth int, touched []bool) {
	if plan == nil {
		return
	}
	if plan.sel != nil {
		sp := plan.sel
		for k := range sp.joins {
			collectTouched(sp.joins[k].on, depth, touched)
		}
		collectTouched(sp.filter, depth, touched)
		collectTouched(sp.having, depth, touched)
		for i := range sp.aggSpecs {
			collectTouched(sp.aggSpecs[i].operand, depth, touched)
		}
		for _, p := range sp.projections {
			collectTouched(p, depth, touched)
		}
		// A materialized ORDER BY expression and a set-returning relation's args / a LATERAL derived
		// body can each carry a correlated reference back into the target scope (the same surfaces
		// selectPlanReferencesOuter checks — query.order_by_correlated, functions.md §10, grammar.md
		// §44). collectTouchedPlan MUST cover every surface that function does, or an outer column read
		// only through one of them is left unfetched by the lazy/masked scan (large-values.md §14) and
		// the correlated subquery re-executes against NULL — a memory-vs-disk divergence.
		for _, oe := range sp.orderExprs {
			collectTouched(oe, depth, touched)
		}
		for i := range sp.rels {
			if sp.rels[i].srf != nil {
				for _, a := range sp.rels[i].srf.args {
					collectTouched(a, depth, touched)
				}
			}
			if sp.rels[i].derived != nil {
				collectTouchedPlan(sp.rels[i].derived, depth+1, touched)
			}
		}
	}
	if plan.values != nil {
		for r := range plan.values.rows {
			for c := range plan.values.rows[r] {
				collectTouched(plan.values.rows[r][c], depth, touched)
			}
		}
	}
	if plan.setop != nil {
		collectTouchedPlan(&plan.setop.lhs, depth, touched)
		collectTouchedPlan(&plan.setop.rhs, depth, touched)
	}
	if plan.with != nil {
		// A nested WITH's correlated references live in its body (the CTE bodies are parent=nil);
		// recurse into the body at the same depth (spec/design/cte.md §7).
		collectTouchedPlan(&plan.with.body, depth, touched)
	}
}

// inMembership is three-valued `lhs IN (list)` membership (spec/design/grammar.md §26), charging
// one operator_eval per element compared. An EMPTY list is `negated` (x IN () = FALSE, x NOT IN ()
// = TRUE) independent of lv. Otherwise: a positive match -> TRUE; else a NULL element (or NULL lv)
// -> NULL; else FALSE. NOT IN is the Kleene negation. Shared by reInValues and the correlated
// reSubquery/sqIn eval.
func inMembership(lv Value, list []Value, negated bool, m *costMeter) (Value, error) {
	if len(list) == 0 {
		return BoolValue(negated), nil
	}
	anyMatch := false
	anyNull := false
	for _, v := range list {
		m.Charge(costs.OperatorEval)
		// Each element comparison over a decimal pair charges its size-scaled decimal_work
		// (spec/design/cost.md §3 "decimal_work"), like a compare node.
		m.Charge(costs.DecimalWork * (decimalCmpWork(lv, v) - 1))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		switch lv.Eq3(v) {
		case True:
			anyMatch = true
		case Unknown:
			anyNull = true
		}
	}
	var inVal Value
	switch {
	case anyMatch:
		inVal = BoolValue(true)
	case anyNull:
		inVal = NullValue()
	default:
		inVal = BoolValue(false)
	}
	if negated {
		return boolNot(inVal), nil
	}
	return inVal, nil
}

// quantifiedMembership is the three-valued membership fold for `lhs op ANY/ALL(array)`
// (array-functions.md §11), the generalization of inMembership to all five comparison operators and
// both quantifiers. A NULL array → NULL; otherwise, over the flattened elements, ANY/SOME (all=false)
// is the OR-fold (TRUE if any `lhs op e` is TRUE, else NULL if any is NULL, else FALSE; empty →
// FALSE) and ALL (all=true) the AND-fold (FALSE if any is FALSE, else NULL if any is NULL, else TRUE;
// empty → TRUE). Each element comparison charges one operator_eval (+ size-scaled decimal_work),
// exactly like inMembership, so max_cost bounds the walk (54P01).
func quantifiedMembership(op binaryOp, all bool, lv, av Value, m *costMeter) (Value, error) {
	if av.Kind == ValNull {
		return NullValue(), nil
	}
	anyNull := false
	for _, e := range av.arrayVal().Elements {
		m.Charge(costs.OperatorEval)
		m.Charge(costs.DecimalWork * (decimalCmpWork(lv, e) - 1))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		switch quantifiedCmp3(op, lv, e) {
		case True:
			// ANY short-circuits TRUE; ALL keeps going (TRUE is its neutral element).
			if !all {
				return BoolValue(true), nil
			}
		case False:
			// ALL short-circuits FALSE; ANY keeps going (FALSE is its neutral element).
			if all {
				return BoolValue(false), nil
			}
		case Unknown:
			anyNull = true
		}
	}
	// Drained without a short-circuit: a NULL seen → UNKNOWN; else the quantifier's identity (ALL →
	// TRUE, ANY → FALSE — also the empty-array result).
	if anyNull {
		return NullValue(), nil
	}
	return BoolValue(all), nil
}

// quantifiedCmp3 is the per-element three-valued comparison `lhs op e` for a quantified node,
// normalizing a mixed-width float pair to f64 first (the resolver admits f32 vs f64,
// matching reCompare's promote — here the array elements are runtime values, so the widen happens per
// element). Bottoms out in the value module's Eq3/Lt3/Gt3 kernels.
//
// A composite operand pair routes through the composite TOTAL ORDER (valueCmp), NOT the bare-ROW 3VL
// Eq3/Lt3/Gt3 (array-functions.md §13): PostgreSQL's = ANY(addr[]) dispatches on the composite =
// operator = record_eq, which is DEFINITE with NULL fields comparable (ROW('a',NULL)::addr =
// ANY(ARRAY[ROW('a',NULL)::addr]) is TRUE), the same total order array_eq / @> already use for
// composite elements (array.md §5). A whole-element NULL is still UNKNOWN — the operator stays strict
// at the value level — so the resolver-guaranteed same-type pair is composite-vs-composite or
// composite-vs-NULL.
func quantifiedCmp3(op binaryOp, x, e Value) ThreeValued {
	if x.Kind == ValComposite || e.Kind == ValComposite {
		if x.Kind == ValNull || e.Kind == ValNull {
			return Unknown
		}
		ord := valueCmp(x, e)
		var matched bool
		switch op {
		case opEq:
			matched = ord == 0
		case opNe:
			matched = ord != 0
		case opLt:
			matched = ord < 0
		case opGt:
			matched = ord > 0
		case opLe:
			matched = ord <= 0
		default: // OpGe
			matched = ord >= 0
		}
		if matched {
			return True
		}
		return False
	}
	if x.Kind == ValFloat32 && e.Kind == ValFloat64 {
		x = Float64Value(float64(x.F32()))
	} else if x.Kind == ValFloat64 && e.Kind == ValFloat32 {
		e = Float64Value(float64(e.F32()))
	}
	switch op {
	case opEq:
		return x.Eq3(e)
	case opNe:
		return not3(x.Eq3(e))
	case opLt:
		return x.Lt3(e)
	case opGt:
		return x.Gt3(e)
	case opLe:
		return or3(x.Lt3(e), x.Eq3(e))
	default: // OpGe
		return or3(x.Gt3(e), x.Eq3(e))
	}
}

// valueToRExpr builds the constant rExpr for a folded subquery value (§26). The static type is
// carried separately (the node's Type), so a NULL value here is just reConstNull.
func valueToRExpr(v Value) *rExpr {
	switch v.Kind {
	case ValInt:
		return &rExpr{kind: reConstInt, cInt: v.Int}
	case ValBool:
		return &rExpr{kind: reConstBool, cBool: v.boolVal()}
	case ValText:
		return &rExpr{kind: reConstText, cText: v.str()}
	case ValDecimal:
		return &rExpr{kind: reConstDecimal, cDec: *v.decimal()}
	case ValBytea:
		return &rExpr{kind: reConstBytea, cBytea: []byte(v.str())}
	case ValUuid:
		return &rExpr{kind: reConstUuid, cBytea: []byte(v.str())}
	case ValTimestamp:
		return &rExpr{kind: reConstTimestamp, cInt: v.Int}
	case ValTimestamptz:
		return &rExpr{kind: reConstTimestamptz, cInt: v.Int}
	case ValDate:
		return &rExpr{kind: reConstDate, cInt: v.Int}
	case ValInterval:
		return &rExpr{kind: reConstInterval, cIv: v.interval()}
	case ValComposite:
		// A folded composite value rebuilds as a ROW(...) of its per-field constant nodes
		// (spec/design/composite.md), so the recursive structure round-trips.
		nodes := make([]*rExpr, len(*v.composite()))
		for i, f := range *v.composite() {
			nodes[i] = valueToRExpr(f)
		}
		return &rExpr{kind: reRow, sargs: nodes}
	case ValArray:
		// A folded array constant — preserve its full shape (dims/lbounds) in a const node.
		return &rExpr{kind: reConstArray, cArray: v.arrayVal()}
	case ValRange:
		// A folded range constant (already canonical).
		return &rExpr{kind: reConstRange, cRange: v.rangeVal()}
	case ValJson:
		return &rExpr{kind: reConstJson, cText: v.str()}
	case ValJsonPath:
		return &rExpr{kind: reConstJsonPath, cText: v.str()}
	case ValJsonb:
		return &rExpr{kind: reConstJsonb, cJsonb: v.jsonb()}
	default: // ValNull
		return &rExpr{kind: reConstNull}
	}
}

// distinctRowKey encodes a projected row into a collision-free string key for DISTINCT
// dedup. Each field carries a type tag (n/i/b) and a payload, joined by a separator that
// no field can contain, so e.g. (1,23) and (12,3) do not collide (spec/design/grammar.md
// §11). NULL == NULL falls out (both encode to "n"), matching the NULL-safe DISTINCT rule.
func distinctRowKey(row []Value) string {
	var b strings.Builder
	for i, v := range row {
		if i > 0 {
			b.WriteByte('|')
		}
		switch v.Kind {
		case ValNull:
			b.WriteByte('n')
		case ValInt:
			b.WriteByte('i')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValBool:
			b.WriteByte('b')
			if v.boolVal() {
				b.WriteByte('1')
			} else {
				b.WriteByte('0')
			}
		case ValText:
			// Length-prefix the content so the separator byte cannot be confused with a
			// text value that contains it (the value bytes are arbitrary UTF-8).
			b.WriteByte('t')
			b.WriteString(strconv.Itoa(len(v.str())))
			b.WriteByte(':')
			b.WriteString(v.str())
		case ValDecimal:
			// Value-canonical key so 1.5 and 1.50 collapse to one DISTINCT bucket
			// (spec/design/decimal.md §5).
			b.WriteByte('d')
			b.WriteString(v.decimal().CanonicalString())
		case ValBytea:
			// Length-prefix the raw bytes (held in Str; a distinct 'y' tag, so a bytea never
			// collides with a text value of the same bytes).
			b.WriteByte('y')
			b.WriteString(strconv.Itoa(len(v.str())))
			b.WriteByte(':')
			b.WriteString(v.str())
		case ValUuid:
			// The 16 raw bytes (held in Str), under a distinct 'u' tag so a uuid never collides
			// with a bytea/text of the same bytes. Fixed-width, but length-prefixed for symmetry.
			b.WriteByte('u')
			b.WriteString(strconv.Itoa(len(v.str())))
			b.WriteByte(':')
			b.WriteString(v.str())
		case ValTimestamp:
			// The i64 microsecond instant (held in Int), under a distinct 's' tag. Two literals
			// for the same instant (e.g. 12:00:00 and 12:00:00.0) share the int, so they bucket
			// together; the infinity sentinels are ordinary int values with their own buckets.
			b.WriteByte('s')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValTimestamptz:
			// The i64 UTC-instant micros (held in Int), under a distinct 'z' tag: offsets are
			// already normalized to UTC at parse, so +00 and +05-of-the-same-instant bucket together.
			b.WriteByte('z')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValDate:
			// The i32 day count (held in Int), under a distinct 'd' tag.
			b.WriteByte('d')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValInterval:
			// The canonical 128-bit span as a decimal string, under a distinct 'v' tag, so
			// span-equal intervals ('1 mon' / '30 days' / '720:00:00') collapse to one DISTINCT/
			// GROUP BY bucket while each value still renders its own fields (spec/design/interval.md §2).
			b.WriteByte('v')
			b.WriteString(v.interval().Span().String())
		case ValFloat32, ValFloat64:
			// Float DISTINCT / GROUP BY uses the §3 total order's equivalence classes: -0 → +0 and
			// ALL NaNs collapse to one bucket (spec/design/float.md §3). Key on the CANONICAL form —
			// a canonical NaN pattern, and +0 for ±0 — so -0/+0 and any two NaNs share a bucket. A
			// distinct 'f' tag (a column is one float width, so the width need not enter the key).
			b.WriteByte('f')
			b.WriteString(floatCanonicalKey(v.asF64()))
		case ValComposite:
			// A composite keys structurally (spec/design/composite.md §2/§5): the field count under a
			// distinct 'c' tag, then each field's own key recursively. NULL fields key as 'n' (the
			// value-level structural equality, like decimal/interval), so two composites with the same
			// field values share a DISTINCT/GROUP BY bucket; a nested composite recurses.
			b.WriteByte('c')
			b.WriteString(strconv.Itoa(len(*v.composite())))
			b.WriteByte(':')
			b.WriteString(distinctRowKey(*v.composite()))
		case ValArray:
			// An array keys structurally INCLUDING its shape (spec/design/array.md §5): the
			// dims and lower bounds (so [2:4]={1,2,3} and {1,2,3} bucket apart — array_eq considers
			// them), then each element's own key recursively. NULL elements key as 'n' (btree
			// equality — NULLs mutually equal), so structurally-equal arrays share a bucket.
			a := v.arrayVal()
			b.WriteByte('a')
			b.WriteString(strconv.Itoa(len(a.Dims)))
			for _, d := range a.Dims {
				b.WriteByte(':')
				b.WriteString(strconv.Itoa(d))
			}
			for _, lb := range a.Lbounds {
				b.WriteByte(';')
				b.WriteString(strconv.FormatInt(int64(lb), 10))
			}
			b.WriteByte('=')
			b.WriteString(distinctRowKey(a.Elements))
		case ValRange:
			// A range keys structurally over its CANONICAL form (PG range btree — spec/design/ranges.md
			// §6), under a distinct 'r' tag: the empty flag, then each bound's presence (infinite = '_'),
			// inclusivity, and the bound value's own key recursively. Because the stored form is canonical,
			// two equal ranges produce the identical key (rangeTotalCmp == 0 ⇔ same key), so they share a
			// DISTINCT/GROUP BY bucket. NULL ranges key as 'n' (the whole-value NULL above).
			rv := v.rangeVal()
			b.WriteByte('r')
			if rv.Empty {
				b.WriteByte('e')
				break
			}
			b.WriteByte('n') // non-empty marker
			writeRangeBoundKey(&b, rv.Lower, rv.LowerInc)
			writeRangeBoundKey(&b, rv.Upper, rv.UpperInc)
		case ValJson:
			// json keys on its verbatim text under a distinct 'J' tag (the value-level equality,
			// consistent with the structural derive). Length-prefixed (arbitrary UTF-8 content).
			// Never reached through SQL in J0 (json is non-comparable — 42883).
			b.WriteByte('J')
			b.WriteString(strconv.Itoa(len(v.str())))
			b.WriteByte(':')
			b.WriteString(v.str())
		case ValJsonb:
			// jsonb keys on its CANONICAL text under a distinct 'B' tag (the canonical form makes
			// structural equality the value equality, §5; jsonbOut is byte-identical for equal trees,
			// so equal jsonb values share a DISTINCT/GROUP BY bucket). Length-prefixed.
			s := jsonbOut(v.jsonb())
			b.WriteByte('B')
			b.WriteString(strconv.Itoa(len(s)))
			b.WriteByte(':')
			b.WriteString(s)
		}
	}
	return b.String()
}

// writeRangeBoundKey appends one canonical range bound to a distinctRowKey buffer: '_' for an
// infinite (nil) bound, else the inclusivity flag ('[' / '(') and the bound value's own recursive key.
func writeRangeBoundKey(b *strings.Builder, bound *Value, inc bool) {
	if bound == nil {
		b.WriteByte('_')
		return
	}
	if inc {
		b.WriteByte('[')
	} else {
		b.WriteByte('(')
	}
	b.WriteString(distinctRowKey([]Value{*bound}))
}

// floatCanonicalKey is a collision-free string of a float's total-order equivalence class
// (spec/design/float.md §3): every NaN → "nan", -0 → +0, otherwise the shortest round-trip
// decimal. So -0/+0 and any two NaNs key identically (they dedup to one DISTINCT/GROUP BY bucket).
func floatCanonicalKey(f float64) string {
	if math.IsNaN(f) {
		return "nan"
	}
	return renderFloat64(canonicalizeFloat64(f))
}
