// Deterministic cost meter (CLAUDE.md §13).
//
// A Meter accrues the execution cost of a query from the shared unit weights in COSTS
// (generated from spec/cost/schedule.toml). The cost of a given (query, database state)
// is fully deterministic and IDENTICAL across every core — a CLAUDE.md §8 divergence
// hotspot, asserted in the conformance corpus. The accrual sites (which executor /
// evaluator / storage line charges which unit) are hand-written here and in executor.ts;
// only the weights are shared data. See spec/design/cost.md.
//
// Every unit routes through the single charge() chokepoint; the caller-set ceiling +
// deterministic abort (spec/design/cost.md §6) is enforced by guard(), consulted at the
// unbounded-work points (per scanned row, per produced row, per expression node, per
// size-scaled decimal_work charge (immediately after it — cost.md §3), and per
// aggregate fold) so a runaway query stops deterministically.

import { engineError } from "./errors.ts";

// Meter accrues deterministic execution cost and enforces an optional ceiling (CLAUDE.md
// §13). Threaded through the executor and the recursive expression evaluator; the accrued
// total is reported on Outcome. The counter is a bigint for i64 parity with the Rust/Go
// cores — a number is f64, which loses integer precision above 2^53 and would silently
// diverge (CLAUDE.md §8).
export class Meter {
  // Total cost accrued so far (CLAUDE.md §13).
  accrued: bigint = 0n;
  // The caller-set cost ceiling, or 0 (the default) for unlimited. A positive value bounds
  // an untrusted query: the instant accrued cost reaches it, the next guard() throws 54P01
  // (spec/design/cost.md §6). Carried from the handle's maxCost setting (spec/design/api.md §8).
  limit: bigint;

  constructor(limit: bigint = 0n) {
    this.limit = limit;
  }

  // charge adds units of cost. The single accrual chokepoint. Enforcement is NOT here:
  // charge only accrues, so the cross-core accrual count (the `# cost:` contract) is
  // untouched; guard() does the comparison at the work loops.
  charge(units: bigint): void {
    this.accrued += units;
  }

  // guard enforces the ceiling: it throws a 54P01 EngineError if a ceiling is set and
  // accrued cost has REACHED it (CLAUDE.md §13 — "the instant accrued cost reaches it,
  // execution aborts"). Called at the unbounded-work points (the scan loop, the produce
  // step, the expression evaluator, the aggregate fold) — the same mirrored points in every
  // core, so the abort is deterministic and cross-core identical (spec/design/cost.md §6).
  // The throw unwinds to the public API boundary, exactly like every other SQL error in the
  // TS core. A no-op (one comparison) when unlimited, so it is free on the hot path by default.
  guard(): void {
    if (this.limit > 0n && this.accrued >= this.limit) {
      throw engineError(
        "cost_limit_exceeded",
        `query exceeded the cost limit of ${this.limit} (accrued ${this.accrued})`,
      );
    }
  }
}
