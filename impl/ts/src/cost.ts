// Deterministic cost meter (CLAUDE.md §13).
//
// A Meter accrues the execution cost of a query from the shared unit weights in COSTS
// (generated from spec/cost/schedule.toml). The cost of a given (query, database state)
// is fully deterministic and IDENTICAL across every core — a CLAUDE.md §8 divergence
// hotspot, asserted in the conformance corpus. The accrual sites (which executor /
// evaluator / storage line charges which unit) are hand-written here and in executor.ts;
// only the weights are shared data. See spec/design/cost.md.
//
// Every unit routes through the single charge() chokepoint, which enforces TWO independent
// ceilings (guard(), consulted at the unbounded-work points — per scanned row, per produced
// row, per expression node, per size-scaled decimal_work charge (immediately after it —
// cost.md §3), and per aggregate fold):
//
//   - Per-statement maxCost → 54P01 (spec/design/cost.md §6): the statement's own accrued
//     cost reaching the caller-set ceiling.
//   - Per-session lifetimeMaxCost → 54P02 (spec/design/session.md §5.4): the session's
//     CUMULATIVE cost reaching the budget. The meter live-charges its units into the
//     session's cumulative total (a shared LifetimeBudget object), so an aborted statement's
//     partial cost counts automatically and the cumulative is session state that survives a
//     transaction rollback.

import { engineError } from "./errors.ts";

// LifetimeBudget is the session lifetime-budget handle a Meter carries (spec/design/session.md
// §5.4): a SHARED object holding the session's cumulative cost total plus the budget. The meter
// charges every unit into `total` (live), so the cumulative is always current — partial cost of an
// aborted statement is already folded in, with no separate end-of-statement step. `limit <= 0` ⇒
// the cumulative is still TRACKED (the gauge stays readable) but NEVER aborts. It is an object (not
// a bare bigint) so the meter and the Session share the same mutable counter by reference.
export class LifetimeBudget {
  // The session's running cumulative cost (spec/design/session.md §5.4). Shared with the Session:
  // the meter live-charges into it, the session reads it back as the lifetimeCost() gauge and checks
  // it at statement admission. A bigint for i64 parity (CLAUDE.md §8).
  total: bigint = 0n;
  // The session's cumulative cost budget, or 0 for unlimited (track-only).
  limit: bigint;

  constructor(limit: bigint = 0n) {
    this.limit = limit;
  }
}

// Meter accrues deterministic execution cost and enforces an optional per-statement ceiling AND an
// optional per-session budget (CLAUDE.md §13; spec/design/session.md §5.4). Threaded through the
// executor and the recursive expression evaluator; the accrued (per-statement) total is reported on
// Outcome, while the session cumulative is updated live through LifetimeBudget. The counters are
// bigint for i64 parity with the Rust/Go cores — a number is f64, which loses integer precision
// above 2^53 and would silently diverge (CLAUDE.md §8).
export class Meter {
  // Total cost accrued so far FOR THIS STATEMENT (CLAUDE.md §13) — the figure reported on Outcome
  // and asserted by the `# cost:` directive.
  accrued: bigint = 0n;
  // The caller-set per-statement cost ceiling, or 0 (the default) for unlimited. A positive value
  // bounds an untrusted query: the instant accrued cost reaches it, the next guard() throws 54P01
  // (spec/design/cost.md §6). Carried from the session's maxCost setting (spec/design/api.md §8).
  limit: bigint;
  // The session lifetime budget (spec/design/session.md §5.4), or undefined for a meter with no
  // session context (the unit-test / scratch-meter path). When present, every charge() also accrues
  // into the session's cumulative total, and guard() throws 54P02 once it reaches the budget.
  lifetime: LifetimeBudget | undefined;

  constructor(limit: bigint = 0n, lifetime?: LifetimeBudget) {
    this.limit = limit;
    this.lifetime = lifetime;
  }

  // charge adds units of cost. The single accrual chokepoint. Accrues into both the per-statement
  // counter (the `# cost:` contract) AND — when a session lifetime budget is attached — the session's
  // cumulative total (live), so partial cost of an aborted statement counts. Enforcement is NOT here:
  // guard() does the comparisons at the work loops, so the cross-core accrual count is untouched.
  charge(units: bigint): void {
    this.accrued += units;
    if (this.lifetime !== undefined) this.lifetime.total += units;
  }

  // guard enforces the ceilings: it throws if the per-statement maxCost (54P01) OR the session
  // lifetimeMaxCost (54P02) has been REACHED (>=, CLAUDE.md §13 — "the instant accrued cost reaches
  // it, execution aborts"). When both are over, the one REACHED FIRST wins — the ceiling crossed at
  // the lower accrued value, i.e. the larger excess; an exact tie breaks to the per-statement 54P01
  // (the inner gate). Called at the unbounded-work points — the same mirrored points in every core,
  // so the abort is deterministic and cross-core identical (spec/design/cost.md §6, session.md §5.4).
  // The throw unwinds to the public API boundary, exactly like every other SQL error in the TS core.
  guard(): void {
    const stmtOver = this.limit > 0n && this.accrued >= this.limit;
    const l = this.lifetime;
    const lifeOver = l !== undefined && l.limit > 0n && l.total >= l.limit;
    if (!stmtOver && !lifeOver) return;
    // Pick the ceiling reached first. Both counters grow in lockstep, so the one crossed at the lower
    // accrued value has the larger excess by the time this guard fires; a tie breaks to the
    // per-statement ceiling.
    let pickLife = lifeOver;
    if (stmtOver && lifeOver && l !== undefined) {
      pickLife = l.total - l.limit > this.accrued - this.limit;
    }
    if (pickLife && l !== undefined) {
      throw engineError(
        "session_cost_limit_exceeded",
        `session exceeded the lifetime cost limit of ${l.limit} (accrued ${l.total})`,
      );
    }
    throw engineError(
      "cost_limit_exceeded",
      `query exceeded the cost limit of ${this.limit} (accrued ${this.accrued})`,
    );
  }
}
