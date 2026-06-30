package jed

// cursor is the pull source a Rows cursor drives (spec/design/streaming.md §4).
//
// S1 ships only the buffered shape: the executor still materializes the full result and the cursor
// walks it one row at a time — today's behavior, byte-unchanged. Its purpose is the seam: Rows is
// now defined in terms of this cursor (nextRow / costAccrued / close) rather than a concrete
// [][]Value + index, so a later streaming shape (streaming.md §4, S3) can plug in without changing
// Rows or any caller. The streaming shape will own the pull B-tree scan cursor (S2) and the pinned
// read snapshot (streaming.md §5), accrue cost as it is pulled (streaming.md §6), and deregister
// its reader-liveness pin in close.
type cursor struct {
	rows [][]Value
	idx  int
	cost int64
}

// bufferedCursor wraps an already-materialized result (the S1 shape).
func bufferedCursor(rows [][]Value, cost int64) *cursor {
	return &cursor{rows: rows, cost: cost}
}

// nextRow pulls the next output row, returning (row, true) or (nil, false) at end. For the buffered
// shape this just advances the index; it is the site where a streaming shape does the per-row work
// (and accrues its cost — streaming.md §6).
func (c *cursor) nextRow() ([]Value, bool) {
	if c.idx >= len(c.rows) {
		return nil, false
	}
	row := c.rows[c.idx]
	c.idx++
	return row, true
}

// costAccrued is the accrued execution cost (CLAUDE.md §13). Final after the cursor is drained
// (streaming.md §6); for the buffered shape it is final immediately (the work is already done).
func (c *cursor) costAccrued() int64 { return c.cost }

// close releases any pinned read snapshot (streaming.md §5). Idempotent. A no-op for the buffered
// shape — it owns a detached [][]Value and pins nothing; the streaming shape (S3) deregisters its
// reader-liveness pin here.
func (c *cursor) close() {}
