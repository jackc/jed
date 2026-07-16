package jed

import (
	"bytes"
	"strings"
	"sync/atomic"
)

// Row production — the scan/stream/join execution engine (the front half of SELECT execution). This
// file holds the streaming query fast path (scanCache/stmtCache/planCacheable, tryScanQuery/
// buildScanRows) and its pull cursors (streamingCursor/bufferedScanCursor/deferredCursor), the
// streaming operators (execStreamingScan/execStreamingSort/execStreamingJoin, execWindowTopN,
// execIndexOrderScan), relation materialization (materializeRel), and the execSelectPlan entry point.
// Projection/emission is in exec_emit.go; access-path selection is in access_path.go.

// snapshotEngine builds a frozen read-snapshot engine for a streaming cursor (spec/design/streaming.md
// §5): the VISIBLE main / session-temp snapshots captured (the snapshots are immutable
// copy-on-write, so sharing the pointers pins the roots cheaply and keeps them stable for the cursor's
// life, isolated from later writes on the live handle) with NO open transaction — so the engine's
// reads see exactly the captured frozen state — plus the session envelope the per-row eval / the cost
// meter read: the cost ceilings + the SHARED lifetime gauge (the *int64 pointer — so streaming cost
// still counts against LifetimeMaxCost), the cancel poll, the entropy/clock seam, session vars, the
// time zone, and the currval/lastval session state. The cursor evaluates its filter/projection against
// this engine, so the streaming Rows is self-contained (it does not reference the live handle, so it
// survives Database.queryValues's transient session, streaming.md §5).
func (db *engine) snapshotEngine() *engine {
	s := db.session // struct copy: shares the seam (func fields), the lifetime gauge (pointer), and the
	// read-only maps (vars/sessionSeq); reset the per-statement / transaction state below.
	s.tx = nil
	s.readPin = nil
	// A scan-lane statement cannot contain a sequence mutator (stmtIsWrite routes it away before this
	// context exists). Nil maps preserve empty-map reads and fail loudly if that invariant is ever
	// broken, while avoiding two per-cursor maps that can never receive a write.
	s.pendingSeq = nil
	s.pendingCurrval = nil
	s.pendingLastName = ""
	s.tempCommitted = db.tempSnap()
	return &engine{
		committed: db.readSnap(),
		session:   s,
		pageSize:  db.pageSize,
		paging:    db.paging,
		path:      db.path,
		spillDir:  db.spillDir,
		readOnly:  db.readOnly,
		// The frozen read engine carries the same pinned attachment view so a streaming read of an
		// attached database (attached-databases.md §5) resolves through it; it never commits (read-only),
		// so it needs no core back-ref. tempCommitted above already froze the temp snapshot.
		attachedCommitted: db.attachReadView(),
	}
}

// scanCache is one immutable filled entry of a prepared statement's plan cache (stmtCache): the
// resolved scan plan + finalized param types, stamped with the exact ordered estimator-input
// signature they were resolved against. Built once, published via stmtCache.p, and never mutated
// after — so a concurrent reader sees a complete entry or none.
type scanCache struct {
	// core identifies the Database the plan was resolved against (including relation-free plans).
	// inputs is the exact P2 relation-scoped estimator signature in source ordinal order.
	core        *sharedCore
	inputs      []estimatorInputSignature
	sp          *selectPlan
	ptys        []scalarType
	plabels     []string
	resultTypes []string
}

// estimatorInputSignature is collision-free by construction: identity and revision are opaque
// pointer-equality tokens owned by the pinned snapshot, never hashes. Catalog generation and table
// name remain explicit fields of the ratified tuple (spec/design/estimator.md §6).
type estimatorInputSignature struct {
	database *estimatorDatabaseIdentity
	catGen   uint64
	table    string
	revision *estimatorRevision
}

// stmtCache memoizes a prepared statement's resolved scan plan + finalized param types so a repeated
// execute skips planning entirely (spec/design/api.md §2.4) — the biggest lever for the point-lookup
// / high-frequency class (planning is ~⅔ of a point lookup's latency and ~88% of its allocations, and
// the resulting GC inflates the tail). An entry is valid only for the Database it was resolved
// against and while every ordered relation signature field still matches: database identity,
// catalog generation, normalized table name, and estimator revision. Filled only from committed
// state and only for a reusable plan (planCacheable + !paramTypes.uncacheable), so reusing it is
// result/plan/cost-identical to a fresh plan.
// Zero value is "empty". The slot is a lock-free atomic pointer: a prepared statement is a standalone
// value shared across sessions — and goroutines — so concurrent executes may race to fill it; the
// entry itself is immutable and last-writer-wins (both candidates are correct for their exact
// input signature; a statement bounced between databases or snapshots merely re-plans).
type stmtCache struct {
	p atomic.Pointer[scanCache]
}

// insertStmtCache is the separately typed prepared-INSERT slot (api.md §2.4). Keeping it separate
// from stmtCache prevents DML metadata from being forced into the SELECT-plan type. The parsed AST
// is immutable, so only one of a PreparedStatement's two slots can ever fill.
type insertStmtCache struct {
	p atomic.Pointer[insertCache]
}

// planCacheable reports whether a resolved scan plan may be memoized on a prepared statement. The
// subquery / precompiled-regex exclusion is tracked separately (paramTypes.uncacheable, set at the
// node's birth — a folded uncorrelated subquery bakes in one execution's params, and a precompiled
// regex carries a per-execution cost flag). Here the relations are vetted: a set-returning / CTE /
// derived relation carries a nested plan or generator we do not vet for reuse, and a temp table has
// no persistent database identity/revision tuple — so a plan referencing any of those is never
// cached (a point lookup / plain join over persistent base tables has none).
func (db *engine) planCacheable(sp *selectPlan) bool {
	for i := range sp.rels {
		r := &sp.rels[i]
		if r.srf != nil || r.cte != nil || r.derived != nil {
			return false
		}
	}
	return !db.planTouchesTemp(sp)
}

// planTouchesTemp reports whether any of the plan's relations currently resolves to a SESSION-LOCAL
// temporary table in THIS session's visible temp domain. Checked at cache fill (a temp plan is never
// cached) and re-checked on every cache HIT: a statement is shared across sessions, and a plan cached
// where a name was persistent must not be served on a session whose temp table shadows that name —
// the temp domain is session-local and intentionally has no cache signature.
// Cheap: one map lookup per relation, against a usually-empty temp catalog.
func (db *engine) planTouchesTemp(sp *selectPlan) bool {
	for i := range sp.rels {
		r := &sp.rels[i]
		if r.db != nil {
			if strings.EqualFold(*r.db, "temp") {
				return true
			}
			continue
		}
		if db.isTempTable(r.tableName) {
			return true
		}
	}
	return false
}

// estimatorInputFor resolves one base relation against this execution's visible pinned snapshots.
// Temp and synthetic/catalog relations are uncacheable until they receive a complete identity.
func (db *engine) estimatorInputFor(r *planRel) (estimatorInputSignature, bool) {
	var snap *snapshot
	if r.db == nil {
		if db.isTempTable(r.tableName) {
			return estimatorInputSignature{}, false
		}
		snap = db.readSnap()
	} else {
		switch strings.ToLower(*r.db) {
		case "temp":
			return estimatorInputSignature{}, false
		case "main":
			snap = db.readSnap()
		default:
			snap = db.attachReadSnap(strings.ToLower(*r.db))
		}
	}
	if snap == nil {
		return estimatorInputSignature{}, false
	}
	table := strings.ToLower(r.tableName)
	if _, ok := snap.table(table); !ok {
		return estimatorInputSignature{}, false
	}
	return estimatorInputSignature{
		database: snap.estimatorIdentity,
		catGen:   snap.catGen,
		table:    table,
		revision: snap.estimatorRevisionFor(table),
	}, true
}

func (db *engine) estimatorInputs(sp *selectPlan) ([]estimatorInputSignature, bool) {
	inputs := make([]estimatorInputSignature, len(sp.rels))
	for i := range sp.rels {
		input, ok := db.estimatorInputFor(&sp.rels[i])
		if !ok {
			return nil, false
		}
		inputs[i] = input
	}
	return inputs, true
}

func (db *engine) estimatorInputsMatch(sp *selectPlan, want []estimatorInputSignature) bool {
	if len(sp.rels) != len(want) {
		return false
	}
	for i := range sp.rels {
		rel, expected := &sp.rels[i], want[i]
		var snap *snapshot
		if rel.db == nil {
			if _, ok := db.tempSnap().tableByKey(expected.table); ok {
				return false
			}
			snap = db.readSnap()
		} else if strings.EqualFold(*rel.db, "temp") {
			return false
		} else if strings.EqualFold(*rel.db, "main") {
			snap = db.readSnap()
		} else {
			snap = db.attachReadSnap(strings.ToLower(*rel.db))
		}
		if snap == nil {
			return false
		}
		if _, ok := snap.tableByKey(expected.table); !ok ||
			snap.estimatorIdentity != expected.database || snap.catGen != expected.catGen ||
			snap.estimatorRevisionForKey(expected.table) != expected.revision {
			return false
		}
	}
	return true
}

// tryScanQuery serves stmt as a lazy STREAMING or BUFFERED query (spec/design/streaming.md §3/§4),
// planning it EXACTLY ONCE and classifying streaming-vs-buffered from that single plan — the
// plan-once replacement for the old tryStreamingQuery + tryBufferedQuery pair, each of which
// re-planned the same statement. Returns (rows, true, nil) for a top-level read SELECT; (nil, false,
// nil) for a shape no scan lane covers (a non-SELECT, a write — incl. a nextval/setval SELECT,
// stmtIsWrite — or a top-level set-op / VALUES / WITH), so the caller falls through to the deferred /
// materialized paths. When sc is non-nil (a prepared statement) a repeated execute over unchanged
// estimator inputs reuses the cached plan and skips planning + the fold; ad-hoc callers pass nil and still
// plan exactly once. The conformance corpus drives this lazy lane for every read (the harness routes
// through queryValues), cross-checked to yield identical rows + total cost as the materialized drive
// under full drain (streaming.md §6).
func (db *engine) tryScanQuery(stmt statement, params []Value, sc *stmtCache) (*Rows, bool, error) {
	if stmt.Select == nil || stmtIsWrite(stmt) {
		return nil, false, nil
	}
	// Cache HIT: the statement still belongs to the same shared Database and every ordered base
	// relation has the same exact identity/generation/name/revision tuple. Resolving those tuples also
	// rejects a session-local temp shadow or a missing/replaced attachment. Reuse the
	// resolved plan + finalized param types — no planQuery, no fold, no param-type walk. A cached plan
	// carries no subquery to fold (planCacheable rejected any), so the shared plan is never mutated;
	// params are still bound per execute inside buildScanRows.
	rsnap := db.readSnap()
	if sc != nil {
		if c := sc.p.Load(); c != nil && c.core == db.core && db.estimatorInputsMatch(c.sp, c.inputs) {
			return db.buildScanRows(c.sp, c.ptys, c.plabels, c.resultTypes, params, false)
		}
	}
	// MISS: plan once.
	ptypes := &paramTypes{}
	plan, err := db.planQuery(queryExpr{Select: stmt.Select}, nil, nil, ptypes)
	if err != nil {
		return nil, false, err
	}
	if plan.sel == nil {
		return nil, false, nil // set-op / VALUES / WITH — a scan lane does not cover it
	}
	sp := plan.sel
	ptys, err := ptypes.finalize()
	if err != nil {
		return nil, false, err
	}
	plabels := paramLabels(len(ptys))
	resultTypes := typeNames(sp.columnTypes)
	// Fill only from committed state, so a working transaction can consume an entry whose exact
	// signature matches but can never publish its working revision into the committed cache slot.
	// Also require a reusable plan and a core identity (a core-less engine never fills).
	inputs, inputsOK := db.estimatorInputs(sp)
	if sc != nil && db.core != nil && rsnap == db.committed && !ptypes.uncacheable && db.planCacheable(sp) && inputsOK {
		sc.p.Store(&scanCache{core: db.core, inputs: inputs, sp: sp, ptys: ptys, plabels: plabels, resultTypes: resultTypes})
	}
	return db.buildScanRows(sp, ptys, plabels, resultTypes, params, true)
}

// buildScanRows binds params, optionally folds uncorrelated subqueries to constants (doFold — done on
// a freshly-planned plan, skipped for a cached one which has no subquery), classifies the plan as
// direct-pull or buffered via pullStreamingScanEligible, and returns the matching lazy cursor. When
// doFold is false, sp is a shared cached plan and stays strictly read-only. The direct branch handles
// full/contiguous-PK scans; generalized bounded streams run through the buffered cursor on first pull.
// Under full drain the rows + total cost are byte-identical to the eager path.
func (db *engine) buildScanRows(sp *selectPlan, ptys []scalarType, plabels, resultTypes []string, params []Value, doFold bool) (*Rows, bool, error) {
	bound, err := bindParamsWithLabels(params, ptys, plabels)
	if err != nil {
		return nil, false, err
	}
	// Fold globally-uncorrelated subqueries to constants (at top level every surviving subquery is
	// uncorrelated) so the per-row eval re-enters nothing — keeping the cursor self-contained. The
	// fold's own cost was already charged to the shared lifetime gauge by its sub-executions; it is
	// added to the cursor's reported cost below (mirroring runQueryExpr's r.cost += …). A cached plan
	// has no subquery, so the fold is skipped (it would be a no-op anyway) — cost stays identical.
	var subqueryCost int64
	if doFold {
		if err := db.foldUncorrelatedInSelect(sp, bound, cteCtx{}, &subqueryCost); err != nil {
			return nil, false, err
		}
	}

	if pullStreamingScanEligible(sp) {
		// Resolve the scan bound (the PK pushdown, if any) and the up-front cost block. An empty bound
		// (e.g. pk = NULL) admits no row.
		b := unboundedBound()
		empty := false
		var pointKey []byte
		isPoint := false
		if sp.phys.relBounds[0] != nil && sp.phys.relBounds[0].pk != nil {
			pointKey, isPoint, empty = db.buildCompletePKPoint(sp.phys.relBounds[0].pk, bound, nil, nil)
			if !isPoint {
				b, empty = db.buildKeyBound(sp.phys.relBounds[0].pk, bound, nil, nil)
			}
		}
		snap := db.snapshotEngine()
		store := snap.lkpStoreScoped(sp.rels[0].db, sp.rels[0].tableName)
		overlap, slabs := 0, 0
		var pointScan *pointStoreScan
		if isPoint && !empty && store.anySpillableTouched(sp.relMasks[0]) {
			row, found, pages, decompress, getErr := store.GetWithUnits(pointKey, sp.relMasks[0])
			if getErr != nil {
				return nil, false, getErr
			}
			overlap, slabs = pages, decompress
			if !found {
				row = nil
			}
			pointScan = store.pointScanPrefetched(row)
		} else if isPoint && !empty {
			overlap = store.pointNodeCount()
			pointScan = store.pointScanDeferred(pointKey)
		} else if isPoint {
			pointScan = store.pointScanPrefetched(nil)
		} else if !empty {
			if overlap, slabs, err = store.OverlapScanUnits(b, sp.relMasks[0]); err != nil {
				return nil, false, err
			}
		}
		meter := snap.session.newMeter()
		meter.Accrued = subqueryCost // the folded constant cost (lifetime already charged)
		meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))

		var offset int64
		if sp.offset != nil {
			offset = *sp.offset
		}
		cur := &streamingCursor{
			eng:      snap,
			plan:     sp,
			env:      &evalEnv{exec: snap, params: bound, outer: nil, rng: newStmtRng(), ctes: cteCtx{}},
			meter:    meter,
			offset:   offset,
			limit:    sp.limit,
			distinct: sp.distinct,
			seen:     make(map[string]bool),
			done:     empty || (sp.limit != nil && *sp.limit == 0),
		}
		if !cur.done {
			if isPoint {
				cur.point = pointScan
			} else {
				cur.scan = store.storeScan(b, sp.phys.pkReverse)
			}
		}
		return &Rows{columnNames: sp.columnNames, columnTypes: resultTypes, cursor: cur}, true, nil
	}

	// Blocking (buffered) shape: buffers its input but yields the output one row at a time via
	// bufferedScanCursor — bounding peak output memory and letting a caller's early exit skip the
	// projection of rows it never pulls (the top-N-over-the-buffer win, streaming.md §4).
	snap := db.snapshotEngine()
	meter := snap.session.newMeter()
	meter.Accrued = subqueryCost // the folded constant cost (lifetime already charged)
	c := &bufferedScanCursor{
		eng:    snap,
		plan:   sp,
		params: bound,
		rng:    newStmtRng(),
		meter:  meter,
	}
	return &Rows{columnNames: sp.columnNames, columnTypes: resultTypes, cursor: c}, true, nil
}

// streamingCursor is the lazy pull pipeline behind a streaming Rows cursor (spec/design/streaming.md
// §3/§4, S3): execStreamingScan's per-row loop turned inside out so the CALLER pulls each row. It
// holds a frozen snapshot engine (eval's exec — so the cursor is self-contained and outlives the
// handle, streaming.md §5), a pull storeScan over that snapshot (the scan pin), the resolved + folded
// plan, an evalEnv, and its own cost meter. Each nextRow runs scan → resolve touched columns → WHERE →
// project for ONE output row, accruing the identical cost units at the identical sites as the eager
// path — so a fully-drained streaming query observes the same rows + total cost (streaming.md §6),
// while a caller that stops early reads (and charges) less.
type streamingCursor struct {
	eng      *engine
	plan     *selectPlan
	env      *evalEnv
	meter    *costMeter
	scan     *storeScan
	point    *pointStoreScan
	offset   int64
	limit    *int64
	distinct bool
	seen     map[string]bool
	passed   int64 // survivors past the filter+dedup so far (OFFSET runs against this)
	produced int64 // output rows produced so far (the LIMIT short-circuit runs against this)
	done     bool  // scan exhausted, LIMIT window full, or empty bound — then nextRow is a no-op
}

func (c *streamingCursor) nextRow() ([]Value, bool, error) {
	if c.done {
		return nil, false, nil
	}
	// The LIMIT short-circuit: once the window is full, stop WITHOUT pulling another row — so no
	// further leaf is faulted (the streaming early-exit win; cost.md §3 "LIMIT short-circuit").
	if c.limit != nil && c.produced >= *c.limit {
		c.done = true
		return nil, false, nil
	}
	for {
		var row storedRow
		var ok bool
		var err error
		if c.point != nil {
			row, ok, err = c.point.next()
		} else {
			_, row, ok, err = c.scan.next()
		}
		if err != nil {
			return nil, false, err
		}
		if !ok {
			c.done = true
			return nil, false, nil
		}
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per scanned row
			return nil, false, err
		}
		c.meter.Charge(costs.StorageRowRead)
		// Materialize the touched columns left unfetched by the lazy load (large-values.md §14); the
		// chain reads were already metered in the up-front block (cost.md §3).
		if c.point != nil {
			row, err = c.point.resolveColumns(row, c.plan.relMasks[0])
		} else {
			row, err = c.scan.resolveColumns(row, c.plan.relMasks[0])
		}
		if err != nil {
			return nil, false, err
		}
		if c.plan.filter != nil {
			v, err := c.plan.filter.eval(row, c.env, c.meter)
			if err != nil {
				return nil, false, err
			}
			if !v.IsTrue() {
				continue
			}
		}
		if c.distinct {
			// DISTINCT (cost.md §3): project EVERY scanned filtered row (the dedup key, charged even
			// for a duplicate — the §3 asymmetry), drop a value already seen, then OFFSET/LIMIT window
			// the survivors — exactly execStreamingScan.
			projected := make([]Value, len(c.plan.projections))
			for i, p := range c.plan.projections {
				v, err := p.eval(row, c.env, c.meter)
				if err != nil {
					return nil, false, err
				}
				projected[i] = v
			}
			if key := distinctRowKey(projected); c.seen[key] {
				continue
			} else {
				c.seen[key] = true
			}
			c.passed++
			if c.passed <= c.offset {
				continue
			}
			c.meter.Charge(costs.RowProduced)
			c.produced++
			return projected, true, nil
		}
		c.passed++
		if c.passed <= c.offset {
			continue
		}
		c.meter.Charge(costs.RowProduced)
		projected := make([]Value, len(c.plan.projections))
		for i, p := range c.plan.projections {
			v, err := p.eval(row, c.env, c.meter)
			if err != nil {
				return nil, false, err
			}
			projected[i] = v
		}
		c.produced++
		return projected, true, nil
	}
}

func (c *streamingCursor) costAccrued() int64 { return c.meter.Accrued }

// close marks the cursor done; the pinned snapshot is owned by eng/scan and reclaimed by the GC, and
// the watermark deregister (if any) lives on the Rows (streaming.md §5). Idempotent.
func (c *streamingCursor) close() { c.done = true }

// tryBufferedQuery tries to serve stmt as a lazy BUFFERED query (spec/design/streaming.md §4, S4) — the
// bufferedScanCursor is the lazy BUFFERED pull pipeline behind a Query Rows cursor for a plan with a
// blocking operator (spec/design/streaming.md §4, S4) — the generalization of the spilling sorter's
// pull iterator to every blocking shape. It owns a frozen snapshot engine (eval's exec — so the cursor
// is self-contained and outlives the handle, streaming.md §5), the resolved + folded plan, bound
// params, a per-statement entropy cell, its own cost meter, and the lazy emission state. On its FIRST
// nextRow it runs the blocking part (execSelectEmit) to completion into an emitter — buffering the input
// (correctly: a sort/group/dedup/join must see it all) and charging the scan/sort/group/dedup cost —
// then yields its buffer ONE row at a time: an emitProject row is projected (and charges row_produced +
// projection) on emission, an emitSorted row is pulled from the `sorted` iterator and projected (the
// streaming-sort output, streaming.md §4/§7), an emitIdentity/emitFinal row is handed out (already
// projected). So peak output memory is one row, a caller's early exit skips the projection of the rows
// it never pulls, and a fully-drained query observes the same rows + total cost as the eager path
// (streaming.md §6).
type bufferedScanCursor struct {
	eng    *engine
	plan   *selectPlan
	params []Value
	rng    *stmtRng
	meter  *costMeter
	ran    bool    // has the blocking part run? (it runs on the first nextRow)
	em     emitter // the emission descriptor, valid once ran
	idx    int64   // next row index: [em.start, em.end) for buffer modes, [0, len) for emitFinal
	done   bool    // exhausted or closed — then nextRow is a no-op
}

func (c *bufferedScanCursor) nextRow() ([]Value, bool, error) {
	if c.done {
		return nil, false, nil
	}
	// Run the blocking part on the FIRST pull (streaming.md §4 — a blocking cursor runs the blocking
	// part then yields its buffer lazily). A mid-blocking cost abort / cancellation / trap surfaces HERE
	// (during iteration), not at Query time (streaming.md §6).
	if !c.ran {
		em, err := c.eng.execSelectEmit(c.plan, nil, c.params, cteCtx{}, c.rng, c.meter)
		if err != nil {
			return nil, false, err
		}
		c.em = em
		c.ran = true
		if em.mode != emitFinal {
			c.idx = em.start
		}
	}
	switch c.em.mode {
	case emitFinal:
		// Already projected + charged — hand the next row out (no further cost).
		if c.idx >= int64(len(c.em.final)) {
			c.done = true
			return nil, false, nil
		}
		row := c.em.final[c.idx]
		c.idx++
		return row, true, nil
	case emitSorted:
		// The streaming sort's lazy output: pull the next windowed row, charge row_produced, and
		// project it (streaming.md §4/§7). The output slice is never built; an early exit (close)
		// releases any undrained spill runs.
		if c.idx >= c.em.end {
			c.done = true
			c.em.sorted.close()
			return nil, false, nil
		}
		row, ok, err := c.em.sorted.next()
		if err != nil {
			return nil, false, err
		}
		if !ok {
			c.done = true
			c.em.sorted.close()
			return nil, false, nil
		}
		c.idx++
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per produced row
			return nil, false, err
		}
		c.meter.Charge(costs.RowProduced)
		env := &evalEnv{exec: c.eng, params: c.params, outer: nil, rng: c.rng, ctes: cteCtx{}}
		projected := make([]Value, len(c.plan.projections))
		for i, p := range c.plan.projections {
			v, perr := p.eval(row, env, c.meter)
			if perr != nil {
				return nil, false, perr
			}
			projected[i] = v
		}
		return projected, true, nil
	case emitIdentity:
		// Pre-projected (the DISTINCT dedup) — charge only row_produced per emitted row.
		if c.idx >= c.em.end {
			c.done = true
			return nil, false, nil
		}
		row := c.em.final[c.idx]
		c.idx++
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per produced row
			return nil, false, err
		}
		c.meter.Charge(costs.RowProduced)
		return row, true, nil
	case emitColumnar:
		// Columnar projection (packed-leaf.md §11 Track A2/A3): gather this row from the dense lanes — a
		// bare-column projection with no full-width row — charging only row_produced (a bare column ref
		// evaluates to a zero-cost slot read, so there is no projection operator_eval to charge). A non-nil
		// `sel` (the A3 filter's survivors) maps output row j to lane position sel[j].
		if c.idx >= c.em.end {
			c.done = true
			return nil, false, nil
		}
		j := c.idx
		c.idx++
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per produced row
			return nil, false, err
		}
		c.meter.Charge(costs.RowProduced)
		li := j
		if c.em.sel != nil {
			li = int64(c.em.sel[j])
		}
		projected := make([]Value, len(c.em.projCols))
		for k, cc := range c.em.projCols {
			projected[k] = c.em.cols[cc][li]
		}
		return projected, true, nil
	default: // emitProject — project the buffer row on emission (charging row_produced + projection)
		if c.idx >= c.em.end {
			c.done = true
			return nil, false, nil
		}
		row := c.em.src[c.idx]
		c.idx++
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per produced row
			return nil, false, err
		}
		c.meter.Charge(costs.RowProduced)
		env := &evalEnv{exec: c.eng, params: c.params, outer: nil, rng: c.rng, ctes: cteCtx{}}
		projected := make([]Value, len(c.plan.projections))
		for i, p := range c.plan.projections {
			v, perr := p.eval(row, env, c.meter)
			if perr != nil {
				return nil, false, perr
			}
			projected[i] = v
		}
		return projected, true, nil
	}
}

func (c *bufferedScanCursor) costAccrued() int64 { return c.meter.Accrued }

// close marks the cursor done; the pinned snapshot is owned by eng and reclaimed by the GC, and the
// watermark deregister (if any) lives on the Rows (streaming.md §5). A `Sorted` emitter additionally
// releases any undrained spill run files (Go has no destructor — streaming.md §5). Idempotent.
func (c *bufferedScanCursor) close() {
	c.done = true
	if c.ran && c.em.mode == emitSorted && c.em.sorted != nil {
		c.em.sorted.close()
	}
}

// tryDeferredQuery tries to serve stmt as a lazy DEFERRED query (spec/design/streaming.md §4/§7) — the
// Query path for a top-level set operation (UNION/INTERSECT/EXCEPT) or pure-query WITH. These are
// blocking shapes whose output is already projected AND charged (no per-row top-level projection to
// defer), so the only streaming win is lazy-yield (streaming.md §7): the cursor defers the whole
// runSetOp / runWith to its FIRST pull — so a 54P01 cost abort, a 54P02 lifetime abort, a canceled
// context, or an arithmetic trap surfaces during iteration, not at Query (§6) — then yields the
// buffered result one row at a time over a frozen snapshot (§5). Returns (nil,false,nil) for any
// non-set-op/WITH statement, or a write-classified one (a data-modifying WITH, a nextval/setval call —
// stmtIsWrite), which falls back to the materialized dispatch path. Under full drain the rows + total
// cost are byte-identical to the materialized drive (it drives the SAME runSetOp / runWith, §6), so the
// corpus — which drives the total queryValues seam — stays green by construction; per-core unit tests
// pin the lazy drive == the materialized drive.
func (db *engine) tryDeferredQuery(stmt statement, params []Value) (*Rows, bool, error) {
	// A write-classified statement (a data-modifying WITH, a sequence mutator) must take the write gate
	// and never streams (streaming.md §7 / sequences.md §4).
	if stmtIsWrite(stmt) {
		return nil, false, nil
	}
	var q deferredQuery
	switch {
	case stmt.SetOp != nil:
		q = deferredQuery{setop: stmt.SetOp}
	case stmt.With != nil:
		q = deferredQuery{with: stmt.With}
	default:
		return nil, false, nil
	}
	// Resolve the output column metadata up front (the Rows cursor exposes it before the first pull).
	// Planning is unmetered + deterministic, so the names/types read here are IDENTICAL to what the
	// deferred run produces (the run reuses runSetOp/runWith verbatim, so there is no rows/cost drift).
	// A planning error (42P01/42804/…) surfaces at Query, matching the eager path.
	names, types, err := db.deferredColumnMeta(stmt)
	if err != nil {
		return nil, false, err
	}
	c := &deferredCursor{eng: db.snapshotEngine(), query: q, params: params}
	return &Rows{columnNames: names, columnTypes: types, cursor: c}, true, nil
}

// deferredColumnMeta resolves the output column names + type names of a top-level set operation /
// pure-query WITH by planning only (no execution) — fills a deferredCursor's Rows metadata before its
// first pull (tryDeferredQuery). Mirrors the planning prefix of runSetOp / runWith exactly so the
// metadata matches the deferred run's. Bound params are not needed: column names/types never depend on
// bound values.
func (db *engine) deferredColumnMeta(stmt statement) ([]string, []string, error) {
	ptypes := &paramTypes{}
	var plan queryPlan
	var err error
	switch {
	case stmt.SetOp != nil:
		plan, err = db.planQuery(queryExpr{SetOp: stmt.SetOp}, nil, nil, ptypes)
	case stmt.With != nil:
		// The planning prefix of runWith (cte.md): plan the CTE bindings, then the body with them
		// visible. The body's columns are the WITH's output columns.
		var bindings []*cteBinding
		bindings, err = db.planCteBindings(stmt.With.Ctes, stmt.With.Recursive, ptypes)
		if err == nil {
			bodyQ := stmt.With.Body.AsQuery() // pure-query WITH (DML excluded by stmtIsWrite)
			plan, err = db.planQuery(*bodyQ, nil, bindings, ptypes)
		}
	default:
		return nil, nil, nil // unreachable: tryDeferredQuery only calls this for SetOp / With
	}
	if err != nil {
		return nil, nil, err
	}
	return plan.columnNames(), typeNames(plan.columnTypes()), nil
}

// deferredQuery is the deferred query payload for a top-level set operation / pure-query WITH (exactly
// one field is set). Run via the eager runSetOp / runWith verbatim so the rows + cost match Execute
// exactly (streaming.md §6).
type deferredQuery struct {
	setop *setOp
	with  *withQuery
}

// deferredCursor is the lazy DEFERRED pull pipeline behind a Query Rows cursor for a top-level set
// operation / pure-query WITH (spec/design/streaming.md §7). It owns a frozen snapshot engine (§5), the
// query AST, and the bound params; on its FIRST nextRow it runs the whole runSetOp / runWith to
// completion (so a cost abort / cancellation / trap surfaces during iteration, not at Query — §6),
// records the accrued cost, and yields the materialized result ONE row at a time. The input is still
// buffered (a set op dedups / a WITH materializes — it must), so the win here is only lazy-yield: the
// work is deferred to the first pull and the result rows are handed out incrementally rather than
// wrapped in an eager outcome. Under full drain the rows + total cost are byte-identical to the eager
// path (it drives the SAME runSetOp / runWith, §6).
type deferredCursor struct {
	eng    *engine
	query  deferredQuery
	params []Value
	ran    bool      // has the query run? (it runs on the first nextRow)
	rows   [][]Value // the materialized result, valid once ran
	idx    int
	cost   int64 // 0 until the first pull runs the query, then selectResult.cost (final)
	done   bool  // exhausted or closed — then nextRow is a no-op
}

func (c *deferredCursor) nextRow() ([]Value, bool, error) {
	if c.done {
		return nil, false, nil
	}
	// Run the whole set op / WITH on the FIRST pull (streaming.md §7), reusing the eager runSetOp /
	// runWith verbatim so the rows + cost match Execute exactly. A mid-run cost abort / cancellation /
	// arithmetic trap surfaces HERE (during iteration), not at Query (streaming.md §6).
	if !c.ran {
		var r selectResult
		var err error
		if c.query.setop != nil {
			r, err = c.eng.runSetOp(c.query.setop, c.params)
		} else {
			r, err = c.eng.runWith(c.query.with, c.params)
		}
		if err != nil {
			return nil, false, err
		}
		c.rows = r.rows
		c.cost = r.cost
		c.ran = true
	}
	if c.idx >= len(c.rows) {
		c.done = true
		return nil, false, nil
	}
	row := c.rows[c.idx]
	c.idx++
	return row, true, nil
}

func (c *deferredCursor) costAccrued() int64 { return c.cost }

// close marks the cursor done + drops any unread rows; the frozen snapshot is owned by eng
// (GC-reclaimed) and the watermark deregister lives on the Rows (streaming.md §5). Idempotent.
func (c *deferredCursor) close() {
	c.done = true
	c.rows = nil
}

func (db *engine) execStreamingScan(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value) (selectResult, error) {
	store := db.lkpStoreScoped(plan.rels[0].db, plan.rels[0].tableName)
	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	out := make([][]Value, 0)
	// DISTINCT (cost.md §3): when the scan already yields ORDER BY order, dedup runs streaming —
	// project EVERY scanned filtered row (the dedup key), drop a value already in `seen` keeping the
	// first (scan-order) occurrence, then the LIMIT/OFFSET window the DISTINCT rows. The sort is
	// elided; the projection is charged per scanned filtered row (the §3 asymmetry).
	seen := make(map[string]bool)
	var passed int64
	visitRow := func(row storedRow, guarded bool) (bool, error) {
		if !guarded {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
				return false, err
			}
		}
		meter.Charge(costs.StorageRowRead)
		// Materialize the touched columns if the lazy load left them unfetched
		// (large-values.md §14) — a fresh copy only when needed (resolveColumns).
		row, err := store.resolveColumns(row, plan.relMasks[0])
		if err != nil {
			return false, err
		}
		if plan.filter != nil {
			v, err := plan.filter.eval(row, env, meter)
			if err != nil {
				return false, err
			}
			if !v.IsTrue() {
				return true, nil
			}
		}
		if plan.distinct {
			// Project per scanned filtered row (the dedup key) and drop duplicates by first
			// occurrence; the OFFSET/LIMIT then window the DISTINCT rows.
			projected := make([]Value, len(plan.projections))
			for i, p := range plan.projections {
				v, err := p.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				projected[i] = v
			}
			if key := distinctRowKey(projected); seen[key] {
				return true, nil // a duplicate of an already-emitted/seen value
			} else {
				seen[key] = true
			}
			passed++
			if passed <= offset {
				return true, nil
			}
			meter.Charge(costs.RowProduced)
			out = append(out, projected)
		} else {
			passed++
			if passed <= offset {
				return true, nil
			}
			meter.Charge(costs.RowProduced)
			projected := make([]Value, len(plan.projections))
			for i, p := range plan.projections {
				v, err := p.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				projected[i] = v
			}
			out = append(out, projected)
		}
		// Stop once a LIMIT window is filled; with no LIMIT, never stop early (emit every
		// survivor after OFFSET, in primary-key scan order).
		if plan.limit != nil {
			return int64(len(out)) < *plan.limit, nil
		}
		return true, nil
	}

	canPull := plan.limit == nil || *plan.limit > 0
	scanTableInterval := func(b keyBound, reverse bool, charge bool) (bool, error) {
		if charge {
			overlap, slabs, err := store.OverlapScanUnits(b, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))
		}
		if !canPull {
			return false, nil
		}
		keepGoing := true
		visit := func(_ []byte, row storedRow) (bool, error) {
			var err error
			keepGoing, err = visitRow(row, false)
			return keepGoing, err
		}
		var err error
		if reverse {
			err = store.ScanRangeRev(b, visit)
		} else {
			err = store.ScanRange(b, visit)
		}
		return keepGoing, err
	}

	rowKeyFromIndex := func(ekey []byte, prefixLen int, suffix []scalarType) []byte {
		at := prefixLen
		for _, ty := range suffix {
			if at < len(ekey) && ekey[at] == 0x01 {
				at++
			} else {
				at += 1 + ty.WidthBytes()
			}
		}
		return ekey[at:]
	}
	scanIndexInterval := func(nameKey string, b keyBound, prefixLen int, suffix []scalarType, charge bool) (bool, error) {
		istore := db.lkpIndexStore(nameKey)
		if charge {
			overlap, slabs, err := istore.OverlapScanUnits(b, nil)
			if err != nil {
				return false, err
			}
			meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))
		}
		if !canPull {
			return false, nil
		}
		keepGoing := true
		err := istore.ScanRange(b, func(ekey []byte, _ storedRow) (bool, error) {
			if err := meter.Guard(); err != nil {
				return false, err
			}
			rowKey := rowKeyFromIndex(ekey, prefixLen, suffix)
			row, ok, pages, slabs, err := store.GetWithUnits(rowKey, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			if !ok {
				panic("an index entry references a stored row")
			}
			meter.Charge(costs.PageRead*int64(pages) + costs.ValueDecompress*int64(slabs))
			keepGoing, err = visitRow(row, true)
			return keepGoing, err
		})
		return keepGoing, err
	}

	sb := plan.phys.relBounds[0]
	switch {
	case sb == nil || sb.pk != nil:
		b, empty := unboundedBound(), false
		if sb != nil {
			b, empty = db.buildKeyBound(sb.pk, params, env.outer, nil)
		}
		if !empty {
			if _, err := scanTableInterval(b, plan.phys.pkReverse, true); err != nil {
				return selectResult{}, err
			}
		}
	case sb.pkSet != nil:
		if canPull {
			intervals := canonicalIntervalSet(sb.pkSet.pkType, sb.pkSet.specs, sb.pkSet.clip, params, env.outer, sb.pkSet.coll, nil)
			for step := 0; step < len(intervals); step++ {
				i := step
				if plan.phys.pkReverse {
					i = len(intervals) - 1 - step
				}
				more, err := scanTableInterval(intervals[i], plan.phys.pkReverse, true)
				if err != nil {
					return selectResult{}, err
				}
				if !more {
					break
				}
			}
		}
	case sb.index != nil:
		b, prefixLen, empty := db.buildIndexBound(sb.index, params, env.outer, nil)
		if !empty {
			if _, err := scanIndexInterval(sb.index.nameKey, b, prefixLen, sb.index.suffixTypes, true); err != nil {
				return selectResult{}, err
			}
		}
	case sb.indexSet != nil:
		if canPull {
			ks := sb.indexSet
			for _, logical := range canonicalIntervalSet(ks.colType, ks.specs, ks.clip, params, env.outer, ks.coll, nil) {
				physical := indexLogicalInterval(logical)
				suffix, prefixLen := ks.tailTypes, 0
				if logical.lo != nil && logical.hi != nil && logical.loInc && logical.hiInc && bytes.Equal(logical.lo, logical.hi) {
					prefixLen = 1 + len(logical.lo)
				} else {
					suffix = append([]scalarType{ks.colType}, suffix...)
				}
				more, err := scanIndexInterval(ks.nameKey, physical, prefixLen, suffix, true)
				if err != nil {
					return selectResult{}, err
				}
				if !more {
					break
				}
			}
		}
	case sb.gin != nil || sb.gist != nil:
		var entries []entry
		var pages, slabs int
		var err error
		if sb.gin != nil {
			var query *rExpr
			if plan.filter != nil {
				if _, q, ok := ginMatch(plan.filter, sb.gin.colGlobal); ok {
					query = q
				}
			}
			entries, pages, slabs, err = db.ginBoundRows(plan.rels[0].tableName, sb.gin, query, nil, env, meter, plan.relMasks[0], true)
		} else {
			var query *rExpr
			if plan.filter != nil {
				if q, ok := gistQueryOperand(plan.filter, sb.gist); ok {
					query = q
				}
			}
			entries, pages, slabs, err = db.gistBoundRows(plan.rels[0].tableName, sb.gist, query, nil, env, meter, plan.relMasks[0], true)
		}
		if err != nil {
			return selectResult{}, err
		}
		// The opclass gather is complete and charged up front. Ordinary entries carry only storage
		// keys; a degenerate full-scan fallback carries the already-fetched rows and its full block.
		meter.Charge(costs.PageRead*int64(pages) + costs.ValueDecompress*int64(slabs))
		if canPull {
			for step := 0; step < len(entries); step++ {
				if err := meter.Guard(); err != nil {
					return selectResult{}, err
				}
				i := step
				if plan.phys.pkReverse {
					i = len(entries) - 1 - step
				}
				e := entries[i]
				row := e.Row
				if row == nil {
					var ok bool
					var n, sl int
					row, ok, n, sl, err = store.GetWithUnits(e.Key, plan.relMasks[0])
					if err != nil {
						return selectResult{}, err
					}
					if !ok {
						panic("an opclass entry references a stored row")
					}
					meter.Charge(costs.PageRead*int64(n) + costs.ValueDecompress*int64(sl))
				}
				more, err := visitRow(row, true)
				if err != nil {
					return selectResult{}, err
				}
				if !more {
					break
				}
			}
		}
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.Accrued}, nil
}

// execWindowTopN serves a windowed top-N (spec/design/window.md §5.2, cost.md §3): a plain window
// query whose LIMIT is answerable from the first OFFSET+LIMIT primary-key-scan rows (the gate is
// windowTopNEligible). It streams the PK scan, applies WHERE, and collects survivors until it has
// OFFSET+LIMIT of them — then runs the ordinary window stage over that PREFIX and emits the
// OFFSET..OFFSET+LIMIT slice. Because every window value at scan position k depends only on rows at
// positions <= k (windowSpecPrefixSafe), and the outer ORDER BY is the PK scan order (pkOrdered) so
// no sort reorders rows, the rows are byte-identical to the eager whole-table path; only the accrued
// cost is lower (fewer rows scanned, filtered, and folded) — the deliberate short-circuit, mirroring
// execStreamingScan's LIMIT stop. page_read is the full block up front (only per-row work
// short-circuits, like the streaming scan).
func (db *engine) execWindowTopN(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value) (emitter, error) {
	store := db.lkpStoreScoped(plan.rels[0].db, plan.rels[0].tableName)

	// The scan bound (the PK pushdown, if any) + its page_read block, exactly as execStreamingScan.
	b := unboundedBound()
	empty := false
	overlap, slabs := 0, 0
	if plan.phys.relBounds[0] != nil && plan.phys.relBounds[0].pk != nil {
		b, empty = db.buildKeyBound(plan.phys.relBounds[0].pk, params, env.outer, nil)
	}
	if !empty {
		var err error
		if overlap, slabs, err = store.OverlapScanUnits(b, plan.relMasks[0]); err != nil {
			return emitter{}, err
		}
	}
	meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))

	limit := *plan.limit // non-nil (windowTopNEligible)
	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	capN := offset + limit
	if capN < offset { // int64 overflow (offset+limit both enormous) ⇒ no effective cap, scan all
		capN = int64(1) << 62
	}

	// Collect the first `cap` surviving rows in PK scan order (respecting pkReverse), charging
	// storage_row_read per scanned row and the WHERE operator_evals — the streaming-scan feed, minus
	// the projection (the window stage runs before projection). Stop the instant `cap` survivors are in
	// hand: a genuine early-out, so the window fold sees only the prefix it needs.
	var rows []storedRow
	if !empty && limit > 0 {
		visit := func(_ []byte, row storedRow) (bool, error) {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
				return false, err
			}
			meter.Charge(costs.StorageRowRead)
			row, err := store.resolveColumns(row, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			if plan.filter != nil {
				v, err := plan.filter.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				if !v.IsTrue() {
					return true, nil
				}
			}
			rows = append(rows, row)
			return int64(len(rows)) < capN, nil // stop once the OFFSET+LIMIT window is filled
		}
		var err error
		if plan.phys.pkReverse {
			err = store.ScanRangeRev(b, visit)
		} else {
			err = store.ScanRange(b, visit)
		}
		if err != nil {
			return emitter{}, err
		}
	}

	// The window stage over the collected prefix — identical to the eager path (§5.2), just fewer rows.
	if err := applyWindowStage(rows, plan.windowSpecs, plan.windowKeys, env, meter); err != nil {
		return emitter{}, err
	}

	// The prefix is already in outer ORDER BY order (pkOrdered), so the sort is elided. Slice the
	// OFFSET..OFFSET+LIMIT window and project on emission — only an emitted row charges row_produced +
	// projection cost (the eager non-DISTINCT window path's emitProject, streaming.md §4).
	n := int64(len(rows))
	start := offset
	if start > n {
		start = n
	}
	end := n
	if limit < n-start {
		end = start + limit
	}
	return emitter{src: rows, start: start, end: end, mode: emitProject}, nil
}

// execIndexOrderScan is the streaming secondary-index-order scan (cost.md §3 "secondary-index
// order"): an ORDER BY the PK scan does NOT satisfy but a B-tree index does, with a LIMIT (the gate
// — plan.indexOrder non-nil). It walks the index store forward in key order, peels the fixed-width
// PK suffix off the END of each entry key (the "key-suffix skip"), point-looks-up the row, applies
// the residual filter, and STOPS once the LIMIT/OFFSET window is filled — a top-N that elides the
// blocking sort (and, for a collated index, the collate units). The index-tree page_read is charged
// up front as the full block (like the streaming PK scan — only the per-row work short-circuits);
// each scanned entry then charges its point-lookup's page_read/value_decompress + one
// storage_row_read, plus row_produced and projection operator_evals per produced row.
func (db *engine) execIndexOrderScan(plan *selectPlan, io *indexOrderPlan, env *evalEnv, meter *costMeter) (selectResult, error) {
	store := db.lkpStoreScoped(plan.rels[0].db, plan.rels[0].tableName)
	istore := db.lkpIndexStore(io.nameKey)
	// Up-front index-tree page_read (the full block; the index store has no payload, so no slabs).
	meter.Charge(costs.PageRead * int64(istore.NodeCount()))

	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	out := make([][]Value, 0)
	if plan.limit == nil || *plan.limit > 0 {
		var passed int64
		err := istore.ScanRange(unboundedBound(), func(ekey []byte, _ storedRow) (bool, error) {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned entry (CLAUDE.md §13)
				return false, err
			}
			// Peel the fixed-width PK suffix off the END of the index entry key (indexes.md §3):
			// the entry key is `<index columns> ‖ storage_key`, and storage_key is exactly
			// io.pkWidth bytes — so the suffix is the row's storage key with no prefix parse.
			rowKey := ekey[len(ekey)-io.pkWidth:]
			row, ok, n, sl, err := store.GetWithUnits(rowKey, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			if !ok {
				panic("an index entry references a stored row")
			}
			meter.Charge(costs.PageRead*int64(n) + costs.ValueDecompress*int64(sl) + costs.StorageRowRead)
			row, err = store.resolveColumns(row, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			if plan.filter != nil {
				v, err := plan.filter.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				if !v.IsTrue() {
					return true, nil
				}
			}
			passed++
			if passed <= offset {
				return true, nil
			}
			meter.Charge(costs.RowProduced)
			projected := make([]Value, len(plan.projections))
			for i, p := range plan.projections {
				v, err := p.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				projected[i] = v
			}
			out = append(out, projected)
			// Stop once a LIMIT window is filled (a top-N over the index order).
			if plan.limit != nil {
				return int64(len(out)) < *plan.limit, nil
			}
			return true, nil
		})
		if err != nil {
			return selectResult{}, err
		}
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.Accrued}, nil
}

// execStreamingSort is the streaming external sort for a single-table ORDER BY (spec/design/spill.md
// §4/§5, streaming.md §4/§7). It streams scan→filter→sorter, so the input is never materialized in the
// executor heap; the sorter spills sorted runs to disk under workMem (file-backed databases) and
// k-way-merges them at finish. It runs the BLOCKING part (scan + sort + the OFFSET skip) and returns an
// emitSorted emitter holding the `sorted` pull iterator positioned at the first output row — so the
// window's row_produced + projection is charged LAZILY by the caller's emitter drive, one row per pull
// (the §4/§7 output-laziness follow-on: the output slice is never built and an early exit skips the rows
// it never pulls). Results + cost under full drain are byte-identical to the eager sort: the same
// page_read block, storage_row_read per scanned row, filter operator_eval, and row_produced per windowed
// row accrue — only the sort, which is unmetered (cost.md §3), now spills. Gated (by the caller) to a
// single table, no join, non-aggregate, non-DISTINCT, with an ORDER BY and no index bound.
func (db *engine) execStreamingSort(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value) (emitter, error) {
	store := db.lkpStoreScoped(plan.rels[0].db, plan.rels[0].tableName)

	// Resolve the scan bound (the PK pushdown, if any) and charge the page_read + value_decompress
	// block up front — identical to the eager scan (cost.md §3). An INDEX bound never reaches here.
	b := unboundedBound()
	empty := false
	overlap, slabs := 0, 0
	if plan.phys.relBounds[0] != nil && plan.phys.relBounds[0].pk != nil {
		b, empty = db.buildKeyBound(plan.phys.relBounds[0].pk, params, env.outer, nil)
	}
	if !empty {
		var err error
		if overlap, slabs, err = store.OverlapScanUnits(b, plan.relMasks[0]); err != nil {
			return emitter{}, err
		}
	}
	meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))

	// Build the sorted source in ORDER BY order, deferring the window's row_produced + projection to
	// the lazy emitter drive (the caller). Two ways to sort, both yielding a `sortedRows` pull iterator
	// over the survivors:
	//
	// A collated ORDER BY cannot use the C-ordered Sorter / spill (collated keys are slice 1e), and
	// collation is in-memory only this slice — so materialize the survivors and sort them with the
	// collation-aware decorate sorter (spec/design/collation.md §8), then wrap the sorted slice as an
	// in-memory `sortedRows`. The metered costs (storage_row_read per scanned row, row_produced per
	// windowed output) are identical to the Sorter path; the sort itself is unmetered like every sort
	// (cost.md §3).
	collated := false
	for _, k := range plan.order {
		if k.collation != nil {
			collated = true
			break
		}
	}
	var total int64
	var sorted *sortedRows
	if collated {
		var rows []storedRow
		if !empty {
			err := store.ScanRange(b, func(_ []byte, row storedRow) (bool, error) {
				if err := meter.Guard(); err != nil {
					return false, err
				}
				meter.Charge(costs.StorageRowRead)
				row, err := store.resolveColumns(row, plan.relMasks[0])
				if err != nil {
					return false, err
				}
				keep := true
				if plan.filter != nil {
					v, err := plan.filter.eval(row, env, meter)
					if err != nil {
						return false, err
					}
					keep = v.IsTrue()
				}
				if keep {
					rows = append(rows, row)
				}
				return true, nil
			})
			if err != nil {
				return emitter{}, err
			}
		}
		total = int64(len(rows))
		if plan.phys.topK != nil {
			var err error
			rows, err = topKRows(rows, plan.order, *plan.phys.topK)
			if err != nil {
				return emitter{}, err
			}
		} else if err := sortRows(rows, plan.order); err != nil {
			return emitter{}, err
		}
		sorted = &sortedRows{mem: rows}
	} else {
		// Stream the scan → filter → sorter. ORDER BY is blocking, so the scan never short-circuits:
		// every in-range row is read (charging storage_row_read), its touched columns resolved
		// (large-values.md §14), the WHERE applied (charging operator_eval), and a survivor pushed into
		// the sorter, which spills when it exceeds the budget.
		useTopK := plan.phys.topK != nil && db.streamingTopKFits(plan, *plan.phys.topK)
		var t *topKKeeper
		var s *sorter
		if useTopK {
			t = newTopKKeeper(*plan.phys.topK, plan.order, false)
		} else {
			s = db.newSorterFor(plan.order)
		}
		var survivorCount int64
		if !empty {
			err := store.ScanRange(b, func(_ []byte, row storedRow) (bool, error) {
				if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
					return false, err
				}
				meter.Charge(costs.StorageRowRead)
				row, err := store.resolveColumns(row, plan.relMasks[0])
				if err != nil {
					return false, err
				}
				keep := true
				if plan.filter != nil {
					v, err := plan.filter.eval(row, env, meter)
					if err != nil {
						return false, err
					}
					keep = v.IsTrue()
				}
				if keep {
					survivorCount++
					if useTopK {
						row = topKPruneUntouched(row, plan.relMasks[0])
						if err := t.push(row); err != nil {
							return false, err
						}
					} else {
						if err := s.push(row); err != nil {
							return false, err
						}
					}
				}
				return true, nil // never stop early — the sort must see every row
			})
			if err != nil {
				return emitter{}, err
			}
		}
		total = survivorCount
		if useTopK {
			sorted = &sortedRows{mem: t.finish()}
		} else {
			var err error
			sorted, err = s.finish()
			if err != nil {
				return emitter{}, err
			}
		}
	}

	// LIMIT / OFFSET window over the sort's total row count (known without materializing the output).
	// Clamp in the i64 domain (CLAUDE.md §8). The OFFSET skip is part of the blocking part (unwindowed —
	// no row_produced), done now so `sorted` is positioned at the first output row; the emitter drive
	// then yields exactly `end-start` rows, charging row_produced + projection per pull (streaming.md
	// §4/§7).
	var start int64
	if plan.offset != nil && *plan.offset < total {
		start = *plan.offset
	} else if plan.offset != nil {
		start = total
	}
	end := total
	if plan.limit != nil && *plan.limit < total-start {
		end = start + *plan.limit
	}
	for i := int64(0); i < start; i++ {
		if _, _, err := sorted.next(); err != nil { // skip the OFFSET rows (unwindowed)
			sorted.close()
			return emitter{}, err
		}
	}
	return emitter{sorted: sorted, start: 0, end: end - start, mode: emitSorted}, nil
}

// streamingTopKFits decides whether a file-backed all-C scan can retain K rows without exceeding
// work_mem's deterministic top-k estimate. An in-memory database (or work_mem=0) has no spill
// contract, so it always uses top-k. File-backed variable/open-type rows have no static upper bound
// and conservatively keep the existing external sorter. Untouched slots are nulled in a private
// retained-row copy; fixed touched scalars use one shared logical
// estimate across cores: 8 bytes per row plus 40 bytes per value (including UUID's payload).
func (db *engine) streamingTopKFits(plan *selectPlan, k int64) bool {
	if k == 0 || db.path == "" || db.session.workMem == 0 {
		return true
	}
	table, ok := db.lkpTableScoped(plan.rels[0].db, plan.rels[0].tableName)
	if !ok {
		return false
	}
	for i, col := range table.Columns {
		if !plan.relMasks[0][i] {
			continue
		}
		ty, scalar := col.Type.AsScalar()
		if !scalar || !topKFixedScalar(ty) {
			return false
		}
	}
	rowBudget := int64(8 + 40*len(table.Columns))
	return k <= int64(db.session.workMem)/rowBudget
}

// topKPruneUntouched releases variable payloads the logical touched set proves no downstream
// filter/order/projection can read. It never mutates a stored shared row.
func topKPruneUntouched(row storedRow, mask []bool) storedRow {
	for _, touched := range mask {
		if !touched {
			out := make(storedRow, len(row))
			copy(out, row)
			for i := range out {
				if !mask[i] {
					out[i] = NullValue()
				}
			}
			return out
		}
	}
	return row
}

func topKFixedScalar(ty scalarType) bool {
	switch ty {
	case scalarInt16, scalarInt32, scalarInt64, scalarBool, scalarUuid, scalarTimestamp,
		scalarTimestamptz, scalarFloat32, scalarFloat64, scalarDate:
		return true
	default:
		return false
	}
}

// execStreamingJoin is a streaming two-table INNER/CROSS join whose ORDER BY is satisfied by the
// OUTER (first) relation's PK scan order (cost.md §3 "JOIN"). The physical join produces
// combined rows in (outer PK, inner key) order — which IS the requested order — so the sort is
// elided, and with a LIMIT the loop STOPS once the window is filled. An ordinary inner is
// materialized once; an index-nested-loop inner is opened per outer row and later seeks are skipped
// when the window fills. Gated (by the caller / plan.joinPkOrdered) to exactly two non-lateral base
// relations, an INNER/CROSS join, a LIMIT, and a forward outer-PK ORDER BY.
func (db *engine) execStreamingJoin(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value, outer []storedRow, rng *stmtRng) (selectResult, error) {
	outerOrdinal := physicalRelOrdinal(plan, 0)
	innerOrdinal := physicalRelOrdinal(plan, 1)
	leftRows, err := db.materializeRel(plan, outerOrdinal, params, outer, nil, rng, env.ctes, meter)
	if err != nil {
		return selectResult{}, err
	}
	rightINL := plan.phys.relINLBounds[innerOrdinal] != nil
	var rightRows []storedRow
	if !rightINL {
		rightRows, err = db.materializeRel(plan, innerOrdinal, params, outer, nil, rng, env.ctes, meter)
		if err != nil {
			return selectResult{}, err
		}
	}
	on := plan.joins[0].on
	var hashTable *hashJoinTable
	if plan.phys.hashJoin != nil && (plan.limit == nil || *plan.limit != 0) {
		hashTable, err = newHashJoinTable(plan.phys.hashJoin, plan.rels[innerOrdinal].offset, plan.rels[outerOrdinal].offset, rightRows, meter)
		if err != nil {
			return selectResult{}, err
		}
	}

	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	out := make([][]Value, 0)
	if plan.limit == nil || *plan.limit > 0 {
		var passed int64
	outerLoop:
		for _, left := range leftRows {
			innerRows := rightRows
			if rightINL {
				outerLogical := placePhysicalRelationRow(plan, outerOrdinal, left)
				innerRows, err = db.materializeRel(plan, innerOrdinal, params, outer, outerLogical, rng, env.ctes, meter)
				if err != nil {
					return selectResult{}, err
				}
			} else if hashTable != nil {
				innerRows, err = hashTable.probe(plan.phys.hashJoin, left, meter)
				if err != nil {
					return selectResult{}, err
				}
			}
			for _, right := range innerRows {
				combined := combinePhysicalRelationRows(plan, outerOrdinal, left, innerOrdinal, right)
				// INNER: keep the pair iff its ON is TRUE (3VL); CROSS: keep every pair (no ON).
				if on != nil {
					v, err := on.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
					if !v.IsTrue() {
						continue
					}
				}
				// The residual WHERE over the combined row (per surviving pair).
				if plan.filter != nil {
					v, err := plan.filter.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
					if !v.IsTrue() {
						continue
					}
				}
				passed++
				if passed <= offset {
					continue
				}
				if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
					return selectResult{}, err
				}
				meter.Charge(costs.RowProduced)
				projected := make([]Value, len(plan.projections))
				for j, p := range plan.projections {
					v, err := p.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
					projected[j] = v
				}
				out = append(out, projected)
				// Stop the whole nested loop once the LIMIT window is filled.
				if plan.limit != nil && int64(len(out)) >= *plan.limit {
					break outerLoop
				}
			}
		}
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.Accrued}, nil
}

func (db *engine) execStreamingNWayJoin(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value, outer []storedRow, rng *stmtRng) (selectResult, error) {
	materialized := make([][]storedRow, len(plan.rels))
	for ordinal, rel := range plan.rels {
		if rel.lateral || plan.phys.relINLBounds[ordinal] != nil {
			continue
		}
		rows, err := db.materializeRel(plan, ordinal, params, outer, nil, rng, env.ctes, meter)
		if err != nil {
			return selectResult{}, err
		}
		materialized[ordinal] = rows
	}
	finalPosition := len(plan.rels) - 1
	running, err := db.execCostedNWayJoin(plan, env, meter, params, outer, materialized, finalPosition-1)
	if err != nil {
		return selectResult{}, err
	}
	inner := plan.phys.relationOrder[finalPosition]
	step := plan.phys.joinSteps[finalPosition-1]
	innerRows := materialized[inner]
	var table *hashJoinTable
	if step.hashJoin != nil {
		table, err = newHashJoinTable(step.hashJoin, plan.rels[inner].offset, 0, innerRows, meter)
		if err != nil {
			return selectResult{}, err
		}
	}
	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	var passed int64
	var out [][]Value
	if plan.limit == nil || *plan.limit > 0 {
	outerLoop:
		for _, left := range running {
			candidates := innerRows
			if plan.phys.relINLBounds[inner] != nil {
				candidates, err = db.materializeRel(plan, inner, params, outer, left, rng, env.ctes, meter)
				if err != nil {
					return selectResult{}, err
				}
			} else if table != nil {
				candidates, err = table.probe(step.hashJoin, left, meter)
				if err != nil {
					return selectResult{}, err
				}
			}
			for _, right := range candidates {
				combined := append(storedRow(nil), left...)
				copy(combined[plan.rels[inner].offset:], right)
				keep := true
				for _, onIndex := range step.onIndices {
					if plan.joins[onIndex].on == nil {
						continue
					}
					v, err := plan.joins[onIndex].on.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
					if !v.IsTrue() {
						keep = false
						break
					}
				}
				if !keep {
					continue
				}
				if plan.filter != nil {
					v, err := plan.filter.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
					if !v.IsTrue() {
						continue
					}
				}
				passed++
				if passed <= offset {
					continue
				}
				if err := meter.Guard(); err != nil {
					return selectResult{}, err
				}
				meter.Charge(costs.RowProduced)
				projected := make([]Value, len(plan.projections))
				for i, p := range plan.projections {
					projected[i], err = p.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
				}
				out = append(out, projected)
				if plan.limit != nil && int64(len(out)) >= *plan.limit {
					break outerLoop
				}
			}
		}
	}
	return selectResult{columnNames: append([]string(nil), plan.columnNames...), columnTypes: append([]resolvedType(nil), plan.columnTypes...), rows: out, cost: meter.Accrued}, nil
}

// newSorterFor builds a sorter for order, bounded by this handle's workMem. Spilling is enabled only
// when the host supplied scratch backing. The file host uses the OS temp directory independently of
// the database path, so read-only filesystems remain readable; in-memory hosts leave it empty and
// never spill (spill.md §2/§4).
func (db *engine) newSorterFor(order []orderSlot) *sorter {
	return newSorter(order, db.session.workMem, db.spillDir)
}

// rowsFromValues reinterprets a result-row slice ([][]Value) as a join-feed buffer ([]Row). Row is
// []Value, so each element converts directly; used where a CTE body's selectResult rows feed the
// join pipeline (spec/design/cte.md §5).
func rowsFromValues(in [][]Value) []storedRow {
	out := make([]storedRow, len(in))
	for i, r := range in {
		out[i] = storedRow(r)
	}
	return out
}

// materializeRel materializes one FROM relation ri into its rows, given the current outer-row stack
// `outer` (spec/design/grammar.md §15/§44). A base table is scanned (a PK/index bound may seek via
// outer); an SRF is generated; a CTE / derived table is delivered / run in place. For a CORRELATED
// LATERAL relation (§44) the caller passes outer EXTENDED with the combined left-hand row, so the
// body / SRF args read that row as their immediate outer; a non-lateral relation is passed the
// query's own outer and its parent=nil body simply ignores it (a parent=nil plan holds no
// outerColumn, so the two are observably identical).
func (db *engine) materializeRel(plan *selectPlan, ri int, params []Value, outer []storedRow, left storedRow, rng *stmtRng, ctes cteCtx, meter *costMeter) ([]storedRow, error) {
	rel := plan.rels[ri]
	env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}
	// A set-returning relation is generated, not scanned (functions.md §10): produce its rows,
	// charging generated_row per element (its args read outer — implicitly lateral, §44).
	if rel.srf != nil {
		switch rel.srf.kind {
		case srfGenerateSeries:
			return db.generateSeriesRows(rel.srf, env, meter)
		case srfUnnest:
			return db.unnestRows(rel.srf, env, meter)
		case srfJsonbArrayElements, srfJsonbArrayElementsText, srfJsonbObjectKeys, srfJsonObjectKeys, srfJsonbEach, srfJsonbEachText, srfJSONRecord, srfJSONRecordset, srfJsonbPathQuery:
			return db.jsonSrfRows(rel.srf, env, meter)
		case srfJsonTable:
			return db.jsonTableRows(rel.srf, env, meter)
		case srfJedTables:
			return db.jedTablesRows(rel.srf, meter)
		case srfJedColumns:
			return db.jedColumnsRows(rel.srf, meter)
		case srfJedIndexes:
			return db.jedIndexesRows(rel.srf, meter)
		case srfJedConstraints:
			return db.jedConstraintsRows(rel.srf, meter)
		case srfJedStatistics:
			return db.jedStatisticsRows(rel.srf, meter)
		}
		return nil, nil
	}
	// A CTE reference delivers its rows from the per-statement context (cte.md §3/§5): a MATERIALIZED
	// CTE reads its buffer (charging cte_scan_row, guarded so a runaway scan aborts 54P01); an INLINE
	// CTE runs its body in place. (A CTE is never lateral.)
	if rel.cte != nil {
		ci := *rel.cte
		switch env.ctes.modes[ci] {
		case cteMaterialize:
			buf := env.ctes.buffers[ci]
			for range buf {
				if err := meter.Guard(); err != nil {
					return nil, err
				}
				meter.Charge(costs.CteScanRow)
			}
			return append([]storedRow(nil), buf...), nil
		case cteInline:
			// Only a plain (query) CTE is ever inlined; a data-modifying CTE is always materialized
			// (writable-cte.md §3), so its buffer was filled above.
			cplan := env.ctes.bindings[ci].plan
			r, err := db.execQueryPlan(&cplan, outer, params, env.ctes)
			if err != nil {
				return nil, err
			}
			meter.Charge(r.cost)
			return rowsFromValues(r.rows), nil
		}
		return nil, nil
	}
	// A DERIVED TABLE runs its body in place (grammar.md §42), charging its intrinsic cost — no
	// cte_scan_row. Non-lateral it was planned parent=nil and ignores outer; a LATERAL body (§44)
	// reads the left-hand row from outer.
	if rel.derived != nil {
		r, err := db.execQueryPlan(rel.derived, outer, params, env.ctes)
		if err != nil {
			return nil, err
		}
		meter.Charge(r.cost)
		return rowsFromValues(r.rows), nil
	}
	// A base table: scan in primary-key order via a scanSource (the page_read block + per-row
	// storage_row_read accrue inside next() — cost.md §3). A PK/index bound seeks/ranges instead of a
	// full walk; an empty bound reads nothing.
	// An index-nested-loop bound (per-outer-row seek) takes precedence over the once-materialized
	// bound and resolves its sibling source from the current left row (cost.md §3 "JOIN"); else the
	// once-materialized relBounds.
	store := db.lkpStoreScoped(rel.db, rel.tableName)
	sb := plan.phys.relINLBounds[ri]
	inl := sb != nil
	if sb == nil {
		sb = plan.phys.relBounds[ri]
	}
	var inlFilters []*rExpr
	var siblingColumns columnRanges
	if inl && len(plan.phys.joinSteps)+1 == len(plan.rels) && len(plan.phys.relationOrder) == len(plan.rels) {
		position := 0
		for i, ordinal := range plan.phys.relationOrder {
			if ordinal == ri {
				position = i
				break
			}
		}
		for _, onIndex := range plan.phys.joinSteps[position-1].onIndices {
			inlFilters = append(inlFilters, plan.joins[onIndex].on)
		}
		for _, ordinal := range plan.phys.relationOrder[:position] {
			r := plan.rels[ordinal]
			siblingColumns = append(siblingColumns, columnRange{start: r.offset, end: r.offset + r.colCount})
		}
	} else if inl && len(plan.phys.relationOrder) == 2 {
		inlFilters = append(inlFilters, plan.joins[0].on)
		siblingColumns = relationColumnRange(plan, physicalRelOrdinal(plan, 0))
	} else if inl {
		inlFilters = append(inlFilters, plan.joins[ri-1].on)
		siblingColumns = columnRanges{{start: 0, end: rel.offset}}
	}
	if inl {
		inlFilters = append(inlFilters, plan.filter)
	}
	var rows []storedRow
	var nodeCount, slabs int
	if sb != nil && sb.index != nil {
		var err error
		if rows, nodeCount, slabs, err = db.indexBoundRows(rel.tableName, sb.index, params, outer, plan.relMasks[ri], left); err != nil {
			return nil, err
		}
	} else if sb != nil && sb.gin != nil {
		// Re-find Q using the same operand class as planning: ON then WHERE for a sibling INL,
		// otherwise the ordinary constant WHERE operand. The full predicate remains residual.
		var query *rExpr
		if inl {
			for _, filter := range inlFilters {
				if _, q, ok := ginSiblingMatch(filter, sb.gin.colGlobal, siblingColumns); ok {
					query = q
					break
				}
			}
		} else if plan.filter != nil {
			if _, q, ok := ginMatch(plan.filter, sb.gin.colGlobal); ok {
				query = q
			}
		}
		entries, pages, sl, err := db.ginBoundRows(rel.tableName, sb.gin, query, left, env, meter, plan.relMasks[ri], false)
		if err != nil {
			return nil, err
		}
		// SELECT discards the storage keys (UPDATE/DELETE keep them — gin.md §6).
		rows = make([]storedRow, len(entries))
		for i := range entries {
			rows[i] = entries[i].Row
		}
		nodeCount, slabs = pages, sl
	} else if sb != nil && sb.gist != nil {
		// Re-find Q using the same operand class as planning. GiST remains always-recheck.
		var query *rExpr
		if inl {
			for _, filter := range inlFilters {
				var q *rExpr
				var ok bool
				if sb.gist.strategy == gistEqual {
					_, q, ok = gistScalarSiblingMatch(filter, sb.gist.colGlobal, siblingColumns)
				} else {
					_, q, ok = gistSiblingMatch(filter, sb.gist.colGlobal, siblingColumns)
				}
				if ok {
					query = q
					break
				}
			}
		} else if plan.filter != nil {
			if q, ok := gistQueryOperand(plan.filter, sb.gist); ok {
				query = q
			}
		}
		entries, pages, sl, err := db.gistBoundRows(rel.tableName, sb.gist, query, left, env, meter, plan.relMasks[ri], false)
		if err != nil {
			return nil, err
		}
		rows = make([]storedRow, len(entries))
		for i := range entries {
			rows[i] = entries[i].Row
		}
		nodeCount, slabs = pages, sl
	} else if sb != nil && sb.pk != nil {
		b, empty := db.buildKeyBound(sb.pk, params, outer, left)
		if !empty {
			entries, pages, sl, err := store.RangeScanWithUnits(b, plan.relMasks[ri])
			if err != nil {
				return nil, err
			}
			rows = make([]storedRow, len(entries))
			for i := range entries {
				rows[i] = entries[i].Row
			}
			nodeCount, slabs = pages, sl
		}
	} else if sb != nil && sb.pkSet != nil {
		// Merged PK point-set (cost.md §3 "OR / IN-list"): a union of point probes over the
		// distinct sorted keys; the whole WHERE stays the residual filter downstream.
		entries, pages, sl, err := db.pkKeySetRows(store, sb.pkSet, params, outer, plan.relMasks[ri], left, true)
		if err != nil {
			return nil, err
		}
		rows = make([]storedRow, len(entries))
		for i := range entries {
			rows[i] = entries[i].Row
		}
		nodeCount, slabs = pages, sl
	} else if sb != nil && sb.indexSet != nil {
		// Merged secondary-index point-set (cost.md §3 "OR / IN-list").
		var err error
		if rows, nodeCount, slabs, err = db.indexKeySetRows(rel.tableName, sb.indexSet, params, outer, plan.relMasks[ri], left); err != nil {
			return nil, err
		}
	} else {
		entries, pages, sl, err := store.ScanWithUnits(plan.relMasks[ri])
		if err != nil {
			return nil, err
		}
		rows = make([]storedRow, len(entries))
		for i := range entries {
			rows[i] = entries[i].Row
		}
		nodeCount, slabs = pages, sl
	}
	// Materialize this relation's touched columns where the lazy load left unfetched references
	// (large-values.md §14) — exactly the static set the cost block charges.
	for i := range rows {
		var err error
		if rows[i], err = store.resolveColumns(rows[i], plan.relMasks[ri]); err != nil {
			return nil, err
		}
	}
	meter.Charge(costs.ValueDecompress * int64(slabs))
	src := &scanSource{rows: rows, nodeCount: nodeCount}
	var tableRows []storedRow
	for {
		row, ok, err := src.next(env, meter)
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		tableRows = append(tableRows, row)
	}
	return tableRows, nil
}

func (db *engine) execSelectPlan(plan *selectPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	// Run the blocking part to an emitter, then drive the emission EAGERLY into a slice (the
	// materialized drive). The lazy queryValues drive walks the SAME emitter row by row via
	// bufferedCursor (streaming.md §4); both charge the identical units at the identical sites, so the
	// totals agree (streaming.md §6).
	rng := newStmtRng()
	meter := db.session.newMeter()
	em, err := db.execSelectEmit(plan, outer, params, ctes, rng, meter)
	if err != nil {
		return selectResult{}, err
	}
	out, err := em.drainEager(db, plan, outer, params, ctes, rng, meter)
	if err != nil {
		return selectResult{}, err
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.Accrued}, nil
}
