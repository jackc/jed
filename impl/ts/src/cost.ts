// Deterministic cost meter (CLAUDE.md §13).
//
// A Meter accrues the execution cost of a query from the shared unit weights in COSTS
// (generated from spec/cost/schedule.toml). The cost of a given (query, database state)
// is fully deterministic and IDENTICAL across every core — a CLAUDE.md §8 divergence
// hotspot, asserted in the conformance corpus. The accrual sites (which executor /
// evaluator / storage line charges which unit) are hand-written here and in executor.ts;
// only the weights are shared data. See spec/design/cost.md.
//
// Every unit routes through the single charge() chokepoint, so the deferred ceiling +
// deterministic abort (spec/design/cost.md §6) is a local, additive change.

// Meter accrues deterministic execution cost. Threaded through the executor and the
// recursive expression evaluator; the accrued total is reported on Outcome. The counter
// is a bigint for int64 parity with the Rust/Go cores — a number is f64, which loses
// integer precision above 2^53 and would silently diverge (CLAUDE.md §8).
export class Meter {
  // Total cost accrued so far (CLAUDE.md §13).
  accrued: bigint = 0n;

  // charge adds units of cost. The single accrual chokepoint: the deferred enforcement
  // (a caller ceiling + deterministic abort) becomes a check inside this method, with no
  // executor call site re-threaded.
  charge(units: bigint): void {
    this.accrued += units;
  }
}
