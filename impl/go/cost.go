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
// Every unit routes through the single Charge chokepoint; the caller-set ceiling +
// deterministic abort (spec/design/cost.md §6) is enforced by Guard, consulted at the
// unbounded-work points (per scanned row, per produced row, per expression node, per
// size-scaled decimal_work charge (immediately after it — cost.md §3), and per
// aggregate fold) so a runaway query stops deterministically.

// Meter accrues deterministic execution cost and enforces an optional ceiling (CLAUDE.md
// §13). Threaded by pointer through the executor and the recursive expression evaluator;
// the accrued total is reported on Outcome.
type Meter struct {
	// Accrued is the total cost so far (CLAUDE.md §13). int64 mirrors the engine's
	// native integer; the ceiling compares against this same counter.
	Accrued int64
	// Limit is the caller-set cost ceiling, or 0 (the default) for unlimited. A positive
	// value bounds an untrusted query: the instant accrued cost reaches it, the next Guard
	// aborts with 54P01 (spec/design/cost.md §6). Carried from the handle's max_cost
	// setting (spec/design/api.md §8).
	Limit int64
}

// NewMeter returns a fresh meter with zero accrued cost and no ceiling.
func NewMeter() *Meter {
	return &Meter{}
}

// NewMeterWithLimit returns a fresh meter that aborts once accrued cost reaches limit
// (limit <= 0 ⇒ unlimited). The ceiling is the handle's max_cost (spec/design/api.md §8).
func NewMeterWithLimit(limit int64) *Meter {
	return &Meter{Limit: limit}
}

// Charge adds units of cost. The single accrual chokepoint. Enforcement is NOT here:
// Charge only accrues, so the cross-core accrual count (the `# cost:` contract) is
// untouched; Guard does the comparison at the work loops.
func (m *Meter) Charge(units int64) {
	m.Accrued += units
}

// Guard enforces the ceiling: it returns a 54P01 error if a ceiling is set and accrued
// cost has REACHED it (CLAUDE.md §13 — "the instant accrued cost reaches it, execution
// aborts"). Called at the unbounded-work points (the scan loop, the produce step, the
// expression evaluator, the aggregate fold) — the same mirrored points in every core, so
// the abort is deterministic and cross-core identical (spec/design/cost.md §6). A no-op
// (one comparison) when unlimited, so it is free on the hot path by default.
func (m *Meter) Guard() error {
	if m.Limit > 0 && m.Accrued >= m.Limit {
		return NewError(CostLimitExceeded, fmt.Sprintf(
			"query exceeded the cost limit of %d (accrued %d)", m.Limit, m.Accrued,
		))
	}
	return nil
}
