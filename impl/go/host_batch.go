package jed

// Host-function batch prefetch (spec/design/extensibility.md §4.2.1) — speculative batch, scalar
// replay. An evaluation site that already holds a chunk of rows asks hostBatch.setRow to call each
// eligible host function ONCE over the chunk's argument columns, then replays its ordinary
// row-at-a-time evaluation with the batch installed on the evalEnv. The reHostFunc eval arm still
// charges, guards, and type-checks per row exactly as before, and evaluates its arguments and
// short-circuits NULL for any row the batch cannot answer; for a prefetched row it takes the kernel's
// outcome from the cache, skipping the argument evaluation that would charge nothing, raise nothing,
// and not short-circuit. So rows, cost, abort points, and errors are identical to batch-of-one by
// construction. Mirrors impl/rust/src/executor/host_batch.rs.

// hostBatchRows is the largest chunk a site prefetches (extensibility.md §4.2.1).
const hostBatchRows = 1024

// hostBatchSlot is one eligible call node's prefetched outcomes over the site's current chunk.
type hostBatchSlot struct {
	// node is the call node's identity (its pointer in the plan the site evaluates) — compared, never
	// followed. A node outside this site's expression list (a subquery's) never matches.
	node *rExpr
	// base is the site row index of the chunk's first row.
	base int
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

// newHostBatch returns the batch for a site that evaluates every expression of exprs once per row,
// or nil when none is eligible (§4.2.1): an item that is itself a call to a non-Volatile host
// function with at least one argument, all trivially evaluable. A zero-argument call is never
// prefetched: its kernel receives no columns, so it could not learn the batch's row count.
func newHostBatch(exprs []*rExpr, ext *ExtensionRegistry) *hostBatch {
	var slots []hostBatchSlot
	for _, e := range exprs {
		if e.kind != reHostFunc || len(e.sargs) == 0 || !ext.function(e.index).batchable() {
			continue
		}
		eligible := true
		for _, a := range e.sargs {
			if !triviallyEvaluable(a) {
				eligible = false
				break
			}
		}
		if eligible {
			slots = append(slots, hostBatchSlot{node: e})
		}
	}
	if len(slots) == 0 {
		return nil
	}
	return &hostBatch{slots: slots, scratch: costMeter{scalarLimit: defaultScalarBytes}}
}

// setRow makes row i of the site current. When a slot's chunk does not cover it, prefetch a new
// chunk starting at i over rows[i:end] (at most hostBatchRows rows, further capped by the meter's
// headroom).
func (b *hostBatch) setRow(i int, rows []storedRow, end int, env *evalEnv, meter *costMeter) {
	b.row = i
	for k := range b.slots {
		slot := &b.slots[k]
		if i < slot.base || i >= slot.base+len(slot.member) {
			headroom, metered := meter.headroom()
			slot.prefetch(i, rows, end, env, &b.scratch, headroom, metered)
		}
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
	// Gather the argument columns over the chunk rows whose arguments are all non-NULL; member maps
	// each such chunk row to its batch row.
	batchRows := 0
rows:
	for k, row := range rows[start : start+n] {
		slot.member[k] = -1
		for j, a := range call.sargs {
			v, err := a.eval(row, env, scratch)
			if err != nil || v.Kind == ValNull {
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
	if batchRows > 0 {
		slot.results, slot.err = hf.callBatch(cols, batchRows)
	}
}
