package jed

// Host-function batch prefetch (spec/design/extensibility.md §4.2.1) — speculative batch, scalar
// replay. An evaluation site that already holds a chunk of rows asks hostBatch.setRow to call each
// eligible host function ONCE over the chunk's argument columns, then replays its ordinary
// row-at-a-time evaluation with the batch installed on the evalEnv. The reHostFunc eval arm still
// charges, guards, and type-checks per row exactly as before, and evaluates its arguments and
// short-circuits NULL for any row the batch cannot answer; for a prefetched row it takes the kernel's
// outcome from the cache, skipping the argument evaluation that would charge nothing, raise nothing,
// and not short-circuit. So rows, cost, abort points, and errors are identical to batch-of-one by
// construction.
//
// A site that holds its rows (the emitProject buffer) prefetches from them directly. A site that
// PULLS its rows (the streaming scan, the streaming sort / spool) first fills a readAhead window from
// its source — pulling is not a metered event there, and a source error is deferred to the row where
// the batch-of-one pull would have raised it (§4.2.1 "In-hand and pulled sites"). Mirrors
// impl/rust/src/executor/host_batch.rs.

// hostBatchRows is the largest chunk a site prefetches (extensibility.md §4.2.1).
const hostBatchRows = 1024

// readAheadRows is the largest read-ahead window a pulled site fills (extensibility.md §4.2.1): every
// pulled row is a fresh allocation the window keeps alive only to batch, so the window stays small —
// still 64× fewer kernel crossings.
const readAheadRows = 64

// hostBatchSlot is one eligible call node's prefetched outcomes over the site's current chunk.
type hostBatchSlot struct {
	// node is the call node's identity (its pointer in the plan the site evaluates) — compared, never
	// followed. A node outside this site's expression list (a subquery's) never matches.
	node *rExpr
	// base is the site row index of the chunk's first row.
	base int
	// valid is how many chunk rows the prefetch settled: the whole chunk, or through the row the
	// kernel reported failing. A row at or past base+valid starts a new chunk.
	valid int
	// member maps each chunk row to its batch row — the index of its result in results — or -1 when the
	// row has no prefetched outcome (a NULL / unevaluable argument) or its outcome was already taken on
	// replay. Its length is the chunk's row count.
	member []int32
	// results holds the kernel's results in batch-row order. When err is non-nil it is the outcome of
	// batch row len(results), and every batch row after it was never computed.
	results []Value
	err     error
	// cols is the argument-column buffer handed to the kernel, reused from chunk to chunk. A kernel may
	// return an argument column as its results (an identity map); that is harmless, since a chunk's
	// results are dead once the next prefetch refills cols.
	cols [][]Value
}

// hostBatch holds the prefetched host-function outcomes of one evaluation site (extensibility.md
// §4.2.1). Owned by one site (one goroutine); never shared.
type hostBatch struct {
	// row is the site's current row index, set by setRow before each row's evaluation.
	row   int
	slots []hostBatchSlot
	// scratch is the meter a prefetch evaluates arguments under; trivially evaluable arguments never
	// charge it.
	scratch costMeter
}

// triviallyEvaluable reports whether e is free and cannot raise, so a prefetch may evaluate it
// without a meter: a column or outer-column reference, a parameter, or a scalar constant.
func triviallyEvaluable(e *rExpr) bool {
	switch e.kind {
	case reColumn, reOuterColumn, reParam,
		reConstInt, reConstBool, reConstText, reConstDecimal, reConstFloat32, reConstFloat64,
		reConstBytea, reConstUuid, reConstJson, reConstJsonb, reConstJsonPath,
		reConstTimestamp, reConstTimestamptz, reConstDate, reConstInterval, reConstNull:
		return true
	default:
		return false
	}
}

// unfetchedArg reports whether a column argument's stored value is still unfetched (a deferred large
// value, large-values.md §14): resolving it is engine I/O that can raise, so the prefetch leaves that
// row to replay (extensibility.md §4.2.1 rule 2).
func unfetchedArg(a *rExpr, row storedRow, env *evalEnv) bool {
	switch a.kind {
	case reColumn:
		return row[a.index].Kind == ValUnfetched
	case reOuterColumn:
		return env.outer[len(env.outer)-a.level][a.index].Kind == ValUnfetched
	default:
		return false
	}
}

// collectHostCalls appends the eligible call nodes reachable from e (extensibility.md §4.2.1): a
// batch-kernel, non-Volatile host call with at least one argument, all trivially evaluable, reached
// only through nodes that evaluate every operand (a cast, unary minus, NOT, IS [NOT] NULL, arithmetic,
// comparison). A zero-argument call is never prefetched: its kernel receives no columns, so it could
// not learn the batch's row count.
func collectHostCalls(e *rExpr, ext *ExtensionRegistry, slots []hostBatchSlot) []hostBatchSlot {
	switch e.kind {
	case reHostFunc:
		if len(e.sargs) == 0 || !ext.function(e.index).batchable() {
			return slots
		}
		for _, a := range e.sargs {
			if !triviallyEvaluable(a) {
				return slots
			}
		}
		return append(slots, hostBatchSlot{node: e})
	case reCast, reNeg, reNot, reIsNull:
		return collectHostCalls(e.operand, ext, slots)
	case reArith, reCompare:
		slots = collectHostCalls(e.lhs, ext, slots)
		return collectHostCalls(e.rhs, ext, slots)
	default:
		return slots
	}
}

// newHostBatch returns the batch for a site that evaluates every expression of exprs once per row,
// or nil when no call in them is eligible (§4.2.1).
func newHostBatch(exprs []*rExpr, ext *ExtensionRegistry) *hostBatch {
	var slots []hostBatchSlot
	for _, e := range exprs {
		slots = collectHostCalls(e, ext, slots)
	}
	if len(slots) == 0 {
		return nil
	}
	return &hostBatch{slots: slots, scratch: costMeter{scalarLimit: defaultScalarBytes}}
}

// setRow makes row i of the site current. When a slot's chunk does not settle it, prefetch a new
// chunk starting at i over rows[i:end] (at most hostBatchRows rows, further capped by the meter's
// headroom).
func (b *hostBatch) setRow(i int, rows []storedRow, end int, env *evalEnv, meter *costMeter) {
	b.row = i
	for k := range b.slots {
		slot := &b.slots[k]
		if i < slot.base || i >= slot.base+slot.valid {
			headroom, metered := meter.headroom()
			slot.prefetch(i, rows, end, env, &b.scratch, headroom, metered)
		}
	}
}

// reset forgets every chunk — the site's row indexes restart (a new read-ahead window). nil-safe.
func (b *hostBatch) reset() {
	if b == nil {
		return
	}
	for k := range b.slots {
		b.slots[k].valid = 0
		b.slots[k].member = b.slots[k].member[:0]
	}
}

// take returns the prefetched outcome of call node `node` for the current row — the kernel's value or
// the error it reported for this row — with ok=true, or ok=false when the batch cannot answer it (the
// caller then calls the kernel for this row alone). A hit proves the row's arguments are all non-NULL.
// nil-safe (a site with no batch).
func (b *hostBatch) take(node *rExpr) (v Value, ok bool, err error) {
	if b == nil {
		return Value{}, false, nil
	}
	for k := range b.slots {
		slot := &b.slots[k]
		if slot.node != node {
			continue
		}
		i := b.row - slot.base
		if i < 0 || i >= len(slot.member) || slot.member[i] < 0 {
			return Value{}, false, nil
		}
		m := int(slot.member[i])
		slot.member[i] = -1
		if m < len(slot.results) {
			return slot.results[m], true, nil
		}
		if m == len(slot.results) && slot.err != nil {
			return Value{}, true, slot.err
		}
		return Value{}, false, nil // a batch row after the one that failed: never computed
	}
	return Value{}, false, nil
}

// prefetch fills this slot's chunk starting at site row start.
func (slot *hostBatchSlot) prefetch(start int, rows []storedRow, end int, env *evalEnv, scratch *costMeter, headroom int64, metered bool) {
	call := slot.node
	hf := env.exec.session.extensions.function(call.index)
	n := min(end-start, hostBatchRows)
	// Every replayed row charges at least the declared cost, so the kernel never runs on more rows
	// than the remaining budget could pay for, plus the one that trips it (§4.2.1).
	if metered && hf.cost > 0 {
		if payable := max(headroom, 0) / hf.cost; payable < int64(n) {
			n = int(payable) + 1
		}
	}
	slot.base = start
	if cap(slot.member) < n {
		slot.member = make([]int32, n)
	}
	slot.member = slot.member[:n]
	if slot.cols == nil {
		slot.cols = make([][]Value, len(call.sargs))
	}
	cols := slot.cols
	for j := range cols {
		if cap(cols[j]) < n {
			cols[j] = make([]Value, 0, n)
		}
		cols[j] = cols[j][:0]
	}
	// Gather the argument columns over the chunk rows whose arguments are all non-NULL and fetched;
	// member maps each such chunk row to its batch row.
	batchRows := 0
rows:
	for k, row := range rows[start : start+n] {
		slot.member[k] = -1
		for j, a := range call.sargs {
			unfetched := unfetchedArg(a, row, env)
			var v Value
			var err error
			if !unfetched {
				v, err = a.eval(row, env, scratch)
			}
			if unfetched || err != nil || v.Kind == ValNull {
				for jj := range j { // drop this row's arguments already gathered
					cols[jj] = cols[jj][:batchRows]
				}
				continue rows
			}
			cols[j] = append(cols[j], v)
		}
		slot.member[k] = int32(batchRows)
		batchRows++
	}
	slot.results, slot.err = nil, nil
	slot.valid = n
	if batchRows > 0 {
		slot.results, slot.err = hf.callBatch(cols, batchRows)
	}
	if slot.err != nil {
		// The chunk settles through the failing row; a later row starts a new chunk.
		for k, m := range slot.member {
			if int(m) == len(slot.results) {
				slot.valid = k + 1
				break
			}
		}
	}
}

// readAhead is a pulled site's read-ahead window (extensibility.md §4.2.1 "In-hand and pulled sites"):
// rows read ahead of replay from a source whose pull is not a metered event, plus the site's batches
// over them. The site hands rows out one at a time with next and evaluates each through its unchanged
// per-row pipeline.
type readAhead struct {
	// rows is the current window, in source order.
	rows []storedRow
	// pos is the window index of the next row to hand out.
	pos int
	// deferred is a source error met while filling the window — raised once every row read before it
	// has been handed out (exactly where the batch-of-one pull would have raised it), dropped if
	// replay stops first.
	deferred error
	// ended is set once the source reported its end.
	ended bool
	// filter is the site's WHERE prefetch, if its predicate has an eligible call.
	filter *hostBatch
	// project is the site's projection prefetch, if a projection item has an eligible call.
	project *hostBatch
}

// newReadAhead returns the window for a site evaluating filter (nil if none) and project per row, or
// nil when neither has an eligible call — the site then pulls row by row as before.
func newReadAhead(filter *rExpr, project []*rExpr, ext *ExtensionRegistry) *readAhead {
	var fb *hostBatch
	if filter != nil {
		fb = newHostBatch([]*rExpr{filter}, ext)
	}
	pb := newHostBatch(project, ext)
	if fb == nil && pb == nil {
		return nil
	}
	return &readAhead{filter: fb, project: pb}
}

// restart points the window at a new source (the next interval of an interval-set scan).
func (r *readAhead) restart() {
	r.rows = r.rows[:0]
	r.pos = 0
	r.deferred = nil
	r.ended = false
}

// next returns the window index of the next source row, refilling the window with up to `window` rows from
// pull once it is spent; ok=false at the source's end; or the deferred source error once every row
// read before it has been handed out.
func (r *readAhead) next(window int, pull func() (storedRow, bool, error)) (int, bool, error) {
	if r.pos == len(r.rows) {
		if err := r.deferred; err != nil {
			r.deferred = nil
			return 0, false, err
		}
		if r.ended {
			return 0, false, nil
		}
		r.rows = r.rows[:0]
		r.pos = 0
		r.filter.reset()
		r.project.reset()
		for len(r.rows) < max(window, 1) {
			row, ok, err := pull()
			if err != nil {
				r.deferred = err
				break
			}
			if !ok {
				r.ended = true
				break
			}
			r.rows = append(r.rows, row)
		}
		if len(r.rows) == 0 {
			err := r.deferred
			r.deferred = nil
			return 0, false, err
		}
	}
	r.pos++
	return r.pos - 1, true, nil
}

// setRow makes window row i current for batch (one of this window's; nil-safe), prefetching from it
// if needed.
func (r *readAhead) setRow(batch *hostBatch, i int, env *evalEnv, meter *costMeter) {
	if batch != nil {
		batch.setRow(i, r.rows, len(r.rows), env, meter)
	}
}

// readAheadCap is the read-ahead window size (extensibility.md §4.2.1): at most readAheadRows; on a
// metered handle at most the rows the remaining budget could pay unit (the per-row charge replay
// makes first) for, plus the one that trips it; and at most remaining (when limited), the rows the
// site can still emit.
func readAheadCap(meter *costMeter, unit int64, remaining int64, limited bool) int {
	n := readAheadRows
	if limited && remaining < int64(n) {
		n = int(max(remaining, 1))
	}
	if h, metered := meter.headroom(); metered && unit > 0 {
		if payable := max(h, 0) / unit; payable < int64(n) {
			n = int(payable) + 1
		}
	}
	return n
}
