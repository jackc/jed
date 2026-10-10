package jed

// Host-function batch prefetch (spec/design/extensibility.md §4.2.1) — speculative batch, scalar
// replay. An evaluation site that already holds a chunk of rows asks hostBatch.setRow to call each
// eligible host function ONCE over the chunk's argument columns, then replays its ordinary
// row-at-a-time evaluation with the batch installed on the evalEnv. The reHostFunc eval arm still
// charges, guards, evaluates its arguments, short-circuits NULL, and type-checks per row exactly as
// before; it only takes the kernel's outcome from the cache instead of calling the kernel. So rows,
// cost, abort points, and errors are identical to batch-of-one by construction. Mirrors
// impl/rust/src/executor/host_batch.rs.

// hostBatchRows is the largest chunk a site prefetches (extensibility.md §4.2.1).
const hostBatchRows = 1024

// hostBatchSlot is one eligible call node's prefetched outcomes over the site's current chunk.
type hostBatchSlot struct {
	// node is the call node's identity (its pointer in the plan the site evaluates) — compared, never
	// followed. A node outside this site's expression list (a subquery's) never matches.
	node *rExpr
	// base is the site row index of outcomes[0].
	base int
	// outcomes holds, per chunk row, the kernel's outcome, or a not-computed outcome (not prefetched —
	// a NULL / unevaluable argument, a row past a reported error). Taken (cleared) on replay.
	outcomes []hostOutcome
}

// hostBatch holds the prefetched host-function outcomes of one evaluation site (extensibility.md
// §4.2.1). Owned by one site (one goroutine); never shared.
type hostBatch struct {
	// row is the site's current row index, set by setRow before each row's evaluation.
	row   int
	slots []hostBatchSlot
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
// function whose arguments are all trivially evaluable.
func newHostBatch(exprs []*rExpr, ext *ExtensionRegistry) *hostBatch {
	var slots []hostBatchSlot
	for _, e := range exprs {
		if e.kind != reHostFunc || !ext.function(e.index).batchable() {
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
	return &hostBatch{slots: slots}
}

// setRow makes row i of the site current. When a slot's chunk does not cover it, prefetch a new
// chunk starting at i over rows[i:end] (at most hostBatchRows rows, further capped by the meter's
// headroom).
func (b *hostBatch) setRow(i int, rows []storedRow, end int, env *evalEnv, meter *costMeter) {
	b.row = i
	headroom, metered := meter.headroom()
	for k := range b.slots {
		slot := &b.slots[k]
		if i < slot.base || i >= slot.base+len(slot.outcomes) {
			slot.prefetch(i, rows, end, env, headroom, metered)
		}
	}
}

// take returns the prefetched outcome of call node `node` for the current row, or ok=false when the
// batch cannot answer it (the caller then calls the kernel for this row alone). nil-safe (a site with
// no batch).
func (b *hostBatch) take(node *rExpr) (hostOutcome, bool) {
	if b == nil {
		return hostOutcome{}, false
	}
	for k := range b.slots {
		slot := &b.slots[k]
		if slot.node != node {
			continue
		}
		i := b.row - slot.base
		if i < 0 || i >= len(slot.outcomes) || !slot.outcomes[i].computed {
			return hostOutcome{}, false
		}
		o := slot.outcomes[i]
		slot.outcomes[i] = hostOutcome{}
		return o, true
	}
	return hostOutcome{}, false
}

// prefetch fills this slot's chunk starting at site row start.
func (slot *hostBatchSlot) prefetch(start int, rows []storedRow, end int, env *evalEnv, headroom int64, metered bool) {
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
	slot.outcomes = make([]hostOutcome, n)
	// Gather the argument columns over the chunk rows whose arguments are all non-NULL; members maps
	// each batch row back to its chunk row.
	cols := make([][]Value, len(call.sargs))
	for j := range cols {
		cols[j] = make([]Value, 0, n)
	}
	members := make([]int, 0, n)
	scratch := newMeter()
	vals := make([]Value, len(call.sargs))
rows:
	for k, row := range rows[start : start+n] {
		for j, a := range call.sargs {
			v, err := a.eval(row, env, scratch)
			if err != nil || v.Kind == ValNull {
				continue rows
			}
			vals[j] = v
		}
		for j, v := range vals {
			cols[j] = append(cols[j], v)
		}
		members = append(members, k)
	}
	if len(members) == 0 {
		return
	}
	for m, o := range hf.callBatch(cols, len(members)) {
		slot.outcomes[members[m]] = o
	}
}
