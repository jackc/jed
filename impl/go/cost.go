package jed

// Deterministic cost meter (CLAUDE.md §13).
//
// A Meter accrues the execution cost of a query from the shared unit weights in Costs
// (generated from spec/cost/schedule.toml). The cost of a given (query, database state)
// is fully deterministic and IDENTICAL across every core — a CLAUDE.md §8 divergence
// hotspot, asserted in the conformance corpus. The accrual sites (which executor /
// evaluator / storage line charges which unit) are hand-written here and in executor.go;
// only the weights are shared data. See spec/design/cost.md.
//
// Every unit routes through the single Charge chokepoint, so the deferred ceiling +
// deterministic abort (spec/design/cost.md §6) is a local, additive change.

// Meter accrues deterministic execution cost. Threaded by pointer through the executor
// and the recursive expression evaluator; the accrued total is reported on Outcome.
type Meter struct {
	// Accrued is the total cost so far (CLAUDE.md §13). int64 mirrors the engine's
	// native integer; a future ceiling compares against this same counter.
	Accrued int64
}

// NewMeter returns a fresh meter with zero accrued cost.
func NewMeter() *Meter {
	return &Meter{}
}

// Charge adds units of cost. The single accrual chokepoint: the deferred enforcement
// (a caller ceiling + deterministic abort) becomes a check inside this method, with no
// executor call site re-threaded.
func (m *Meter) Charge(units int64) {
	m.Accrued += units
}
