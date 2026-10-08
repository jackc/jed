package jed

import (
	"errors"
	"fmt"
	"math"
	"sync/atomic"
)

// Deterministic cost meter (CLAUDE.md §13).
//
// A Meter accrues the execution cost of a query from the shared unit weights in Costs
// (generated from spec/cost/schedule.toml). The cost of a given (query, database state)
// is fully deterministic and IDENTICAL across every core — a CLAUDE.md §8 divergence
// hotspot, asserted in the conformance corpus. The accrual sites (which executor /
// evaluator / storage line charges which unit) are hand-written here and in executor.go;
// only the weights are shared data. See spec/design/cost.md.
//
// Every unit routes through the single Charge chokepoint, which enforces TWO independent
// ceilings (Guard, consulted at the unbounded-work points — per scanned row, per produced
// row, per expression node, per size-scaled decimal_work charge (immediately after it —
// cost.md §3), and per aggregate fold):
//
//   - Per-statement max_cost → 54P01 (spec/design/cost.md §6): the statement's own accrued
//     cost reaching the caller-set ceiling.
//   - Per-session lifetime_max_cost → 54P02 (spec/design/session.md §5.4): the session's
//     CUMULATIVE cost reaching the budget. The meter live-charges its units into the
//     session's cumulative total (a shared *int64), so an aborted statement's partial cost
//     counts automatically and the cumulative is session state that survives a rollback.

// Meter accrues deterministic execution cost and enforces an optional per-statement ceiling
// AND an optional per-session budget (CLAUDE.md §13; spec/design/session.md §5.4). Threaded by
// pointer through the executor and the recursive expression evaluator; the accrued
// (per-statement) total is reported on outcome, while the session cumulative is updated live.
type costMeter struct {
	ceilingHit uint8 // first crossed ceiling: 1 statement, 2 lifetime

	scalarBytes *int64
	scalarLimit int64
	// query is the statement's live query-memory account (spec/design/memory.md §2), shared by
	// every meter of the statement like scalarBytes; an unlimited account never computes a size.
	query queryAccount

	// Accrued is the total cost so far FOR THIS STATEMENT (CLAUDE.md §13) — the figure reported
	// on outcome and asserted by the `# cost:` directive. i64 mirrors the engine's native
	// integer; the per-statement ceiling compares against this counter.
	Accrued int64
	// Limit is the caller-set per-statement cost ceiling, or 0 (the default) for unlimited. A
	// positive value bounds an untrusted query: the instant accrued cost reaches it, the next
	// Guard aborts with 54P01 (spec/design/cost.md §6). Carried from the session's max_cost
	// setting (spec/design/api.md §8).
	Limit int64
	// lifetimeTotal points at the session's running CUMULATIVE cost (spec/design/session.md §5.4),
	// or nil for a meter with no session context (the unit-test / build-meter path). When set,
	// every Charge live-adds into it, so partial cost of an aborted statement counts.
	lifetimeTotal *int64
	// lifetimeLimit is the session's cumulative cost budget, or 0 for unlimited (track-only). When
	// positive, Guard aborts 54P02 once *lifetimeTotal reaches it.
	lifetimeLimit int64
	// cancel is an optional cancellation poll (spec/design/api.md §11.4): when set and it returns
	// true, the next Guard aborts the statement with 57014 query_canceled. It rides this same
	// chokepoint so a host's cancellation handle (Go context.Context, …) interrupts a long-running
	// statement at the next metering point — NOT only at the cursor boundary. nil ⇒ no cancellation
	// (the default; zero overhead, and the path every conformance / cost test takes — cost is
	// unaffected, the §8 determinism contract intact). The poll is a single atomic load (armCancel).
	cancel func() bool
}

// NewMeter returns a fresh meter with zero accrued cost, no ceiling, and no session context.
func newMeter() *costMeter {
	return &costMeter{scalarLimit: defaultScalarBytes}
}

// unmetered reports that no enforcement is armed — no per-statement ceiling, no session lifetime
// budget, and no cancellation poll — so Guard is a pure no-op. The vectorized fast paths (batch.go)
// gate on this: with nothing to abort, they may bulk-charge a whole scan's units at once and skip
// per-row Guard, and the accrued total still matches the row-at-a-time path exactly (CLAUDE.md §8).
// A metered meter keeps the scalar path, so its deterministic abort row is unchanged.
func (m *costMeter) unmetered() bool {
	return m.Limit == 0 && m.lifetimeLimit == 0 && m.cancel == nil
}

// NewMeterWithLimit returns a fresh meter that aborts once accrued cost reaches limit
// (limit <= 0 ⇒ unlimited), with no session lifetime budget. The ceiling is the session's
// max_cost (spec/design/api.md §8). Used where there is no session cumulative to thread.
func newMeterWithLimit(limit int64) *costMeter {
	return &costMeter{Limit: limit, scalarLimit: defaultScalarBytes}
}

// Charge adds units of cost. The single accrual chokepoint. Accrues into both the per-statement
// counter (the `# cost:` contract) AND — when a session lifetime budget is attached — the
// session's cumulative total (live), so partial cost of an aborted statement counts. Enforcement
// is NOT here: Guard does the comparisons at the work loops, so the cross-core accrual count is
// untouched.
func (m *costMeter) Charge(units int64) {
	if m.ceilingHit == 0 {
		stmt := m.Limit > 0 && units >= m.Limit-m.Accrued
		life := m.lifetimeTotal != nil && m.lifetimeLimit > 0 && units >= m.lifetimeLimit-*m.lifetimeTotal
		if stmt {
			m.ceilingHit = 1
		}
		if life && (!stmt || m.lifetimeLimit-*m.lifetimeTotal < m.Limit-m.Accrued) {
			m.ceilingHit = 2
		}
	}

	m.Accrued = saturatingCostAdd(m.Accrued, units)
	if m.lifetimeTotal != nil {
		*m.lifetimeTotal = saturatingCostAdd(*m.lifetimeTotal, units)
	}
}

// Guard enforces the ceilings: it aborts if the per-statement max_cost (54P01) OR the session
// lifetime_max_cost (54P02) has been REACHED (>=, CLAUDE.md §13 — "the instant accrued cost
// reaches it, execution aborts"). When both are over, the one REACHED FIRST wins — the ceiling
// crossed at the lower accrued value, i.e. the larger excess; an exact tie breaks to the
// per-statement 54P01 (the inner gate). Called at the unbounded-work points — the same mirrored
// points in every core, so the abort is deterministic and cross-core identical (spec/design/cost.md
// §6, spec/design/session.md §5.4). A no-op (one or two comparisons) when both are unlimited.
func (m *costMeter) Guard() error {
	// Cancellation is checked first and independently of the cost ceilings: a flipped token aborts
	// regardless of accrued cost (spec/design/api.md §11.4). The nil check short-circuits when no
	// cancellation handle is armed, so the cost accrual and the cross-core abort points are
	// unchanged (CLAUDE.md §8) — this never fires in the conformance/cost suites.
	if m.cancel != nil && m.cancel() {
		return newError(QueryCanceled, "canceling statement due to user request")
	}
	stmtOver := m.Limit > 0 && m.Accrued >= m.Limit
	lifeOver := m.lifetimeTotal != nil && m.lifetimeLimit > 0 && *m.lifetimeTotal >= m.lifetimeLimit
	if !stmtOver && !lifeOver {
		return nil
	}
	// Pick the ceiling reached first. Both counters grow in lockstep, so the one crossed at the
	// lower accrued value has the larger excess by the time this guard fires; a tie breaks to the
	// per-statement ceiling.
	pickLife := lifeOver
	if stmtOver && lifeOver {
		pickLife = (*m.lifetimeTotal - m.lifetimeLimit) > (m.Accrued - m.Limit)
	}
	if m.ceilingHit != 0 {
		pickLife = m.ceilingHit == 2
	}
	if pickLife {
		return newError(SessionCostLimitExceeded, fmt.Sprintf(
			"session exceeded the lifetime cost limit of %d (accrued %d)", m.lifetimeLimit, *m.lifetimeTotal,
		))
	}
	return newError(CostLimitExceeded, fmt.Sprintf(
		"query exceeded the cost limit of %d (accrued %d)", m.Limit, m.Accrued,
	))
}

func saturatingCostAdd(a, b int64) int64 {
	if b > math.MaxInt64-a {
		return math.MaxInt64
	}
	return a + b
}

// ReserveScalar admits cumulative logical scalar allocation before constructing it.
// Zero means the finite default; a host cannot accidentally disable this backstop.
func (m *costMeter) ReserveScalar(bytes int64) error {
	if m.scalarBytes == nil {
		m.scalarBytes = new(int64)
	}
	limit := m.scalarLimit
	if limit <= 0 {
		limit = defaultScalarBytes
	}
	if bytes > limit-*m.scalarBytes {
		return newError(ScalarMemoryLimitExceeded, fmt.Sprintf("scalar allocations exceeded the limit of %d bytes", limit))
	}
	*m.scalarBytes += bytes
	return nil
}

// queryMemoryUnderflows counts releases that exceeded their account's balance — always an engine
// accounting bug. Read by the conformance harness's whole-corpus accounting mode
// (QueryMemoryUnderflows).
var queryMemoryUnderflows atomic.Uint64

// QueryMemoryUnderflows reports how many query-memory releases have exceeded their account's
// balance in this process (spec/design/memory.md §5) — an engine accounting bug detector for the
// conformance harness. Always 0 in a correct engine.
func QueryMemoryUnderflows() uint64 { return queryMemoryUnderflows.Load() }

// queryMemoryPeak is the highest balance any account has reached since the conformance harness last
// reset it — the minimal passing max_query_memory_bytes of the record just run.
var queryMemoryPeak atomic.Int64

// QueryMemoryPeak reports the highest query-memory balance reached since ResetQueryMemoryPeak — the
// conformance harness's per-record peak mode compares it across cores (spec/design/memory.md §7).
func QueryMemoryPeak() int64 { return queryMemoryPeak.Load() }

// ResetQueryMemoryPeak zeroes the peak before the harness runs a record.
func ResetQueryMemoryPeak() { queryMemoryPeak.Store(0) }

// queryAccount is a statement's live query-memory account (spec/design/memory.md §2): the shared
// running total and the budget (limit <= 0 ⇒ unlimited). Copied into every meter of the statement
// and into the cursor that outlives it, so all of them reserve against one total.
type queryAccount struct {
	used  *int64
	limit int64
}

// active reports whether the account has a finite budget. Every admission site tests this first,
// so the unlimited default computes no sizes.
func (a queryAccount) active() bool { return a.limit > 0 && a.used != nil }

// reserve adds bytes, or fails 54P05 when used + bytes > limit (equality allowed).
func (a queryAccount) reserve(bytes int64) error {
	if a.tryReserve(bytes) {
		return nil
	}
	return newError(QueryMemoryLimitExceeded, fmt.Sprintf("query memory exceeded the limit of %d bytes", a.limit))
}

// tryReserve adds bytes if they fit; false (nothing reserved) when used + bytes > limit.
func (a queryAccount) tryReserve(bytes int64) bool {
	if !a.active() {
		return true
	}
	if bytes > a.limit-*a.used {
		return false
	}
	*a.used += bytes
	if *a.used > queryMemoryPeak.Load() {
		queryMemoryPeak.Store(*a.used)
	}
	return true
}

// release returns bytes; never errors, never below zero. A release larger than the balance is an
// accounting bug: it clamps and is counted for the conformance harness.
func (a queryAccount) release(bytes int64) {
	if !a.active() {
		return
	}
	if bytes > *a.used {
		queryMemoryUnderflows.Add(1)
		*a.used = 0
		return
	}
	*a.used -= bytes
}

// admitRow admits a projected row (memory.md §5.1).
func (a queryAccount) admitRow(row []Value) error {
	if !a.active() {
		return nil
	}
	return a.reserve(memRowBytes(row))
}

// releaseRow releases a projected row leaving engine ownership (memory.md §5.3).
func (a queryAccount) releaseRow(row []Value) {
	if a.active() {
		a.release(memRowBytes(row))
	}
}

// measureRows is the bytes of a buffer of projected rows under account a, or 0 when unlimited
// (nothing is measured).
func measureRows[R ~[]Value](a queryAccount, rows []R) int64 {
	if !a.active() {
		return 0
	}
	var n int64
	for _, r := range rows {
		n += memRowBytes(r)
	}
	return n
}

// reserveQuery reserves bytes of live query memory, or fails 54P05. A rejected reservation consults
// the meter's cost guard first, so a step that has already reached a cost ceiling reports the cost
// error (memory.md §2) — even where the charging site itself does not guard. A successful
// reservation adds no guard point.
func (m *costMeter) reserveQuery(bytes int64) error {
	if err := m.query.reserve(bytes); err != nil {
		if cerr := m.Guard(); cerr != nil {
			return cerr
		}
		return err
	}
	return nil
}

// queryMemoryActive reports whether the statement has a finite query-memory budget.
func (m *costMeter) queryMemoryActive() bool { return m.query.active() }

// releaseQuery returns bytes of live query memory; never errors, never below zero.
func (m *costMeter) releaseQuery(bytes int64) { m.query.release(bytes) }

// admitRow admits a projected row appended to a row buffer (memory.md §5.1).
func (m *costMeter) admitRow(row []Value) error {
	if !m.query.active() {
		return nil
	}
	return m.reserveQuery(memRowBytes(row))
}

// admitRowMasked admits a pre-projection row under the plan's touched mask (memory.md §3).
func (m *costMeter) admitRowMasked(row []Value, mask []bool) error {
	if !m.query.active() {
		return nil
	}
	return m.reserveQuery(memRowBytesMasked(row, mask))
}

// admitValue admits a value appended to a buffered row (memory.md §5.1).
func (m *costMeter) admitValue(v Value) error {
	if !m.query.active() {
		return nil
	}
	return m.reserveQuery(memValueBytes(v))
}

// releaseRow releases a discarded projected row.
func (m *costMeter) releaseRow(row []Value) {
	if m.query.active() {
		m.query.release(memRowBytes(row))
	}
}

// releaseRowMasked releases a discarded pre-projection row under the plan's touched mask.
func (m *costMeter) releaseRowMasked(row []Value, mask []bool) {
	if m.query.active() {
		m.query.release(memRowBytesMasked(row, mask))
	}
}

// releaseRows releases a whole discarded buffer of projected rows.
func releaseRows[R ~[]Value](m *costMeter, rows []R) {
	if m.query.active() {
		m.query.release(measureRows(m.query, rows))
	}
}

// releaseRowsMasked releases a whole discarded buffer of pre-projection rows under the touched mask.
func releaseRowsMasked[R ~[]Value](m *costMeter, rows []R, mask []bool) {
	if m.query.active() {
		var n int64
		for _, r := range rows {
			n += memRowBytesMasked(r, mask)
		}
		m.query.release(n)
	}
}

// stateCharge is the query-memory charge an operator-state structure holds (spec/design/memory.md
// §6): the bytes it has reserved against the statement's account and not yet returned. Go has no
// destructors, so every owner calls releaseAll at its spec'd release point (and on its error paths
// where the statement continues). Inert (and free) when the account is unlimited; the zero value is
// an inert charge.
type stateCharge struct {
	acct queryAccount
	held int64
}

func newStateCharge(acct queryAccount) *stateCharge { return &stateCharge{acct: acct} }

// active reports whether the account has a finite budget — callers skip measuring otherwise.
func (c *stateCharge) active() bool { return c != nil && c.acct.active() }

// reserve reserves bytes through meter, so a rejection consults the cost guard first (memory.md
// §6.7).
func (c *stateCharge) reserve(meter *costMeter, bytes int64) error {
	if !c.active() {
		return nil
	}
	if err := meter.reserveQuery(bytes); err != nil {
		return err
	}
	c.held += bytes
	return nil
}

// reserveDirect reserves bytes straight against the account, for a structure with no meter in
// reach. The caller passes the outcome through costMeter.costFirst (memory.md §6.7).
func (c *stateCharge) reserveDirect(bytes int64) error {
	if !c.active() {
		return nil
	}
	if err := c.acct.reserve(bytes); err != nil {
		return err
	}
	c.held += bytes
	return nil
}

// tryReserveDirect reserves bytes straight against the account if they fit, for a spill-capable
// structure: false (nothing reserved) tells it to spill instead of failing 54P05 (memory.md §6.6).
func (c *stateCharge) tryReserveDirect(bytes int64) bool {
	if !c.active() {
		return true
	}
	if !c.acct.tryReserve(bytes) {
		return false
	}
	c.held += bytes
	return true
}

// release returns bytes of this structure's charge.
func (c *stateCharge) release(bytes int64) {
	if !c.active() {
		return
	}
	c.acct.release(bytes)
	c.held -= bytes
}

// releaseAll returns everything this structure still holds.
func (c *stateCharge) releaseAll() {
	if c != nil && c.held != 0 {
		c.acct.release(c.held)
		c.held = 0
	}
}

// stateCharge returns a fresh operator-state charge against this statement's account (memory.md §6).
func (m *costMeter) stateCharge() *stateCharge { return newStateCharge(m.query) }

// costFirst applies the cost-wins rule to a reservation made without this meter (a spill structure,
// sorter, or top-k heap reserving through its stateCharge, memory.md §6.7): a 54P05 first consults
// the cost guard, so a step that already reached a cost ceiling reports the cost error. Sound at any
// point on the error's way out, because accrued cost only grows and nothing is charged in between.
func (m *costMeter) costFirst(err error) error {
	if err == nil {
		return nil
	}
	var e *EngineError
	if errors.As(err, &e) && e.State == QueryMemoryLimitExceeded {
		if cerr := m.Guard(); cerr != nil {
			return cerr
		}
	}
	return err
}
