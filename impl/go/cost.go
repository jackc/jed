package jed

import "fmt"

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
// (per-statement) total is reported on Outcome, while the session cumulative is updated live.
type costMeter struct {
	// Accrued is the total cost so far FOR THIS STATEMENT (CLAUDE.md §13) — the figure reported
	// on Outcome and asserted by the `# cost:` directive. i64 mirrors the engine's native
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
	return &costMeter{}
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
	return &costMeter{Limit: limit}
}

// Charge adds units of cost. The single accrual chokepoint. Accrues into both the per-statement
// counter (the `# cost:` contract) AND — when a session lifetime budget is attached — the
// session's cumulative total (live), so partial cost of an aborted statement counts. Enforcement
// is NOT here: Guard does the comparisons at the work loops, so the cross-core accrual count is
// untouched.
func (m *costMeter) Charge(units int64) {
	m.Accrued += units
	if m.lifetimeTotal != nil {
		*m.lifetimeTotal += units
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
	if pickLife {
		return newError(SessionCostLimitExceeded, fmt.Sprintf(
			"session exceeded the lifetime cost limit of %d (accrued %d)", m.lifetimeLimit, *m.lifetimeTotal,
		))
	}
	return newError(CostLimitExceeded, fmt.Sprintf(
		"query exceeded the cost limit of %d (accrued %d)", m.Limit, m.Accrued,
	))
}
