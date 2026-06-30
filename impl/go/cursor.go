package jed

// cursor is the pull source a Rows cursor drives (spec/design/streaming.md §4).
//
// Three shapes, chosen by the plan (streaming.md §4):
//   - bufCursor — a fully materialized result, walked one row at a time. The executor ran the query
//     to completion (the Execute path the conformance corpus drives, byte-unchanged); cost is final.
//     A set-operation / WITH top level (a deferred S4 follow-on) and the materialized fallback land here.
//   - streamingCursor (S3, executor.go) — a lazy pull pipeline for the single-table no-blocking-op
//     scan: scan → resolve → WHERE → project, ONE row per nextRow over a pinned snapshot, accruing
//     cost as it is pulled (streaming.md §6). Peak memory is one row; a caller that stops early faults
//     no further leaves.
//   - bufferedScanCursor (S4, executor.go) — a lazy pull pipeline for a blocking plan (non-PK ORDER BY,
//     DISTINCT, aggregate, window, join): the input buffers (on the first pull) but the OUTPUT is
//     yielded one row at a time, bounding peak output memory and short-circuiting a caller's early exit.
type cursor interface {
	// nextRow pulls the next output row, (row, true, nil) or (nil, false, nil) at end. A streaming
	// cursor may return a non-nil error mid-drain (a 54P01 cost abort, a canceled context, or an
	// arithmetic trap) — surfaced as the statement's error (streaming.md §6).
	nextRow() (row []Value, ok bool, err error)
	// costAccrued is the accrued execution cost (CLAUDE.md §13). Final after the cursor is drained
	// (streaming.md §6); for the buffered shape it is final immediately.
	costAccrued() int64
	// close releases any pinned read snapshot (streaming.md §5). Idempotent.
	close()
}

// bufCursor wraps an already-materialized result (the buffered shape).
type bufCursor struct {
	rows [][]Value
	idx  int
	cost int64
}

// bufferedCursor wraps an already-materialized result.
func bufferedCursor(rows [][]Value, cost int64) cursor {
	return &bufCursor{rows: rows, cost: cost}
}

func (c *bufCursor) nextRow() ([]Value, bool, error) {
	if c.idx >= len(c.rows) {
		return nil, false, nil
	}
	row := c.rows[c.idx]
	c.idx++
	return row, true, nil
}

func (c *bufCursor) costAccrued() int64 { return c.cost }

// close is a no-op for the buffered shape — it owns a detached [][]Value and pins nothing.
func (c *bufCursor) close() {}
