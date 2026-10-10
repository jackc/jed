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

import { DEFAULT_SCALAR_BYTES } from "./costs.ts";
import { EngineError, engineError } from "./errors.ts";
import { rowBytes, rowBytesMasked, valueBytes } from "./memsize.ts";
import type { Value } from "./value.ts";

// queryMemoryUnderflows counts releases that exceeded their account's balance — always an engine
// accounting bug. Read by the conformance harness's whole-corpus accounting mode
// (JED_CONFORMANCE_QUERY_MEMORY), which fails the record that caused it.
export const queryMemoryUnderflows = { count: 0 };

// queryMemoryPeak is the highest balance any account has reached since the conformance harness last
// reset it — the minimal passing maxQueryMemoryBytes of the record just run (memory.md §8).
export const queryMemoryPeak = { value: 0 };

// QueryAccount is a statement's live query-memory account (spec/design/memory.md §2): the running
// total and the budget (limit <= 0 ⇒ unlimited). Shared BY REFERENCE by every meter of the statement
// and by the cursor that outlives it, so all of them reserve against one total. Plain numbers: the
// logical bytes stay far below 2^53 (a huge limit is clamped to Number.MAX_SAFE_INTEGER).
export class QueryAccount {
  used = 0;
  // floor is the balance the account opened with — its transaction's pending writes (memory.md §7).
  // The statement's releases return only what it reserved, so none may take used below it.
  floor = 0;
  readonly limit: number;

  constructor(limit = 0) {
    this.limit = limit;
  }

  // active reports whether the statement has a finite budget. Every admission site tests this first,
  // so the unlimited default computes no sizes.
  active(): boolean {
    return this.limit > 0;
  }

  // reserve adds bytes, or throws 54P05 when used + bytes > limit (equality allowed).
  reserve(bytes: number): void {
    if (this.tryReserve(bytes)) return;
    throw engineError(
      "query_memory_limit_exceeded",
      `query memory exceeded the limit of ${this.limit} bytes`,
    );
  }

  // tryReserve adds bytes if they fit; false (nothing reserved) when used + bytes > limit.
  tryReserve(bytes: number): boolean {
    if (this.limit <= 0) return true;
    if (bytes > this.limit - this.used) return false;
    this.used += bytes;
    if (this.used > queryMemoryPeak.value) queryMemoryPeak.value = this.used;
    return true;
  }

  // release returns bytes; never throws, never below the opening balance (floor). A release past the
  // statement's own balance is an accounting bug (a release without its reservation): clamp, and
  // count it.
  release(bytes: number): void {
    if (this.limit <= 0) return;
    if (bytes > this.used - this.floor) {
      queryMemoryUnderflows.count++;
      this.used = Math.min(this.floor, this.used);
      return;
    }
    this.used -= bytes;
  }

  // admitRow admits a projected row (memory.md §5.1).
  admitRow(row: Value[]): void {
    if (this.limit > 0) this.reserve(rowBytes(row));
  }

  // releaseRow releases a projected row leaving engine ownership (memory.md §5.3).
  releaseRow(row: Value[]): void {
    if (this.limit > 0) this.release(rowBytes(row));
  }

  // measureRows is the bytes of a buffer of projected rows, or 0 when unlimited (nothing measured).
  measureRows(rows: Value[][]): number {
    if (this.limit <= 0) return 0;
    let n = 0;
    for (let i = 0; i < rows.length; i++) n += rowBytes(rows[i]!);
    return n;
  }
}

// StateCharge is the query-memory charge an operator-state structure holds (spec/design/memory.md §6):
// the bytes it has reserved against the statement's account and not yet returned. It is released
// explicitly at the structure's spec'd release point (TS has no destructors, so every owner names that
// point). Inert (and free) when the account is unlimited.
export class StateCharge {
  readonly acct: QueryAccount;
  held = 0;

  constructor(acct: QueryAccount = UNLIMITED_QUERY_ACCOUNT) {
    this.acct = acct;
  }

  // active reports whether the account has a finite budget — callers skip measuring otherwise.
  active(): boolean {
    return this.acct.limit > 0;
  }

  // reserve reserves bytes through meter, so a rejection consults the cost guard first (memory.md §6.7).
  reserve(meter: Meter, bytes: number): void {
    if (this.acct.limit <= 0) return;
    meter.reserveQuery(bytes);
    this.held += bytes;
  }

  // reserveDirect reserves bytes straight against the account, for a structure with no meter in reach;
  // the caller wraps the operation in Meter.costFirst (memory.md §6.7).
  reserveDirect(bytes: number): void {
    if (this.acct.limit <= 0) return;
    this.acct.reserve(bytes);
    this.held += bytes;
  }

  // tryReserveDirect reserves bytes straight against the account if they fit, for a spill-capable
  // structure: false (nothing reserved) tells it to spill instead of throwing 54P05 (memory.md §6.6).
  tryReserveDirect(bytes: number): boolean {
    if (this.acct.limit <= 0) return true;
    if (!this.acct.tryReserve(bytes)) return false;
    this.held += bytes;
    return true;
  }

  // release returns bytes of this structure's charge.
  release(bytes: number): void {
    if (this.acct.limit <= 0) return;
    this.acct.release(bytes);
    this.held -= bytes;
  }

  // releaseAll returns everything this structure still holds.
  releaseAll(): void {
    if (this.held !== 0) {
      this.acct.release(this.held);
      this.held = 0;
    }
  }
}

// UNLIMITED_QUERY_ACCOUNT is the default account of a meter with no session context: unlimited, so it
// is never mutated (every method returns before touching `used`).
export const UNLIMITED_QUERY_ACCOUNT = new QueryAccount(0);

// newQueryAccount starts a statement's account for the host setting: non-positive ⇒ the shared
// unlimited account (never mutated, so no allocation on the default path); a huge value clamps to
// Number.MAX_SAFE_INTEGER.
export function newQueryAccount(bytes: bigint): QueryAccount {
  if (bytes <= 0n) return UNLIMITED_QUERY_ACCOUNT;
  return new QueryAccount(
    bytes > BigInt(Number.MAX_SAFE_INTEGER) ? Number.MAX_SAFE_INTEGER : Number(bytes),
  );
}

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
  private ceilingHit = 0;
  scalarBytes = { used: 0n };
  scalarLimit = DEFAULT_SCALAR_BYTES;
  reserveScalar(bytes: bigint): void {
    const limit = this.scalarLimit > 0n ? this.scalarLimit : DEFAULT_SCALAR_BYTES;
    if (bytes > limit - this.scalarBytes.used)
      throw engineError(
        "scalar_memory_limit_exceeded",
        `scalar allocations exceeded the limit of ${limit} bytes`,
      );
    this.scalarBytes.used += bytes;
  }

  // The statement's live query-memory account (spec/design/memory.md §2), shared by every meter of
  // the statement like scalarBytes; an unlimited account never computes a size.
  query: QueryAccount = UNLIMITED_QUERY_ACCOUNT;

  // queryMemoryActive reports whether the statement has a finite query-memory budget. Every admission
  // site tests this first, so the unlimited default computes no sizes.
  queryMemoryActive(): boolean {
    return this.query.limit > 0;
  }
  // reserveQuery reserves bytes of live query memory, or throws 54P05 (equality allowed). A rejected
  // reservation consults the meter's cost guard first, so a step that has already reached a cost
  // ceiling reports the cost error (memory.md §2) — even where the charging site itself does not
  // guard. A successful reservation adds no guard point.
  reserveQuery(bytes: number): void {
    try {
      this.query.reserve(bytes);
    } catch (e) {
      this.guard();
      throw e;
    }
  }
  // releaseQuery returns bytes of live query memory; never throws, never below zero.
  releaseQuery(bytes: number): void {
    this.query.release(bytes);
  }
  // costFirst runs `fn` — work that reserves without this meter (a spill structure, sorter, or top-k
  // heap reserving through its StateCharge, memory.md §6.7) — and applies the cost-wins rule to a 54P05
  // it raises: the cost guard is consulted first, so a step that already reached a cost ceiling reports
  // the cost error. Sound at any point on the error's way out, because accrued cost only grows.
  costFirst<T>(fn: () => T): T {
    try {
      return fn();
    } catch (e) {
      if (e instanceof EngineError && e.state === "query_memory_limit_exceeded") this.guard();
      throw e;
    }
  }
  // stateCharge is a fresh operator-state charge against this statement's account (memory.md §6).
  stateCharge(): StateCharge {
    return new StateCharge(this.query);
  }
  // admitRow admits a projected row appended to a row buffer (memory.md §5.1).
  admitRow(row: Value[]): void {
    if (this.query.limit > 0) this.reserveQuery(rowBytes(row));
  }
  // admitRowMasked admits a pre-projection row under the plan's touched mask (memory.md §3).
  admitRowMasked(row: Value[], mask: boolean[]): void {
    if (this.query.limit > 0) this.reserveQuery(rowBytesMasked(row, mask));
  }
  // admitValue admits a value appended to a buffered row (memory.md §5.1).
  admitValue(v: Value): void {
    if (this.query.limit > 0) this.reserveQuery(valueBytes(v));
  }
  // releaseRow releases a discarded projected row.
  releaseRow(row: Value[]): void {
    if (this.query.limit > 0) this.query.release(rowBytes(row));
  }
  // releaseRowMasked releases a discarded pre-projection row under the plan's touched mask.
  releaseRowMasked(row: Value[], mask: boolean[]): void {
    if (this.query.limit > 0) this.query.release(rowBytesMasked(row, mask));
  }
  // releaseRows releases a whole discarded buffer of projected rows.
  releaseRows(rows: Value[][]): void {
    if (this.query.limit > 0) {
      let n = 0;
      for (let i = 0; i < rows.length; i++) n += rowBytes(rows[i]!);
      this.query.release(n);
    }
  }
  // releaseRowsMasked releases a whole discarded buffer of pre-projection rows under the mask.
  releaseRowsMasked(rows: Value[][], mask: boolean[]): void {
    if (this.query.limit > 0) {
      let n = 0;
      for (let i = 0; i < rows.length; i++) n += rowBytesMasked(rows[i]!, mask);
      this.query.release(n);
    }
  }

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
    if (this.ceilingHit === 0) {
      const stmt = this.limit > 0n && units >= this.limit - this.accrued;
      const l = this.lifetime;
      const life = l !== undefined && l.limit > 0n && units >= l.limit - l.total;
      if (stmt) this.ceilingHit = 1;
      if (life && l !== undefined && (!stmt || l.limit - l.total < this.limit - this.accrued))
        this.ceilingHit = 2;
    }

    this.accrued = saturatingCostAdd(this.accrued, units);
    if (this.lifetime !== undefined)
      this.lifetime.total = saturatingCostAdd(this.lifetime.total, units);
  }

  // headroom is the cost still payable before a ceiling trips — the smaller remaining per-statement /
  // lifetime budget — or undefined when neither is armed. Bounds how far a host-function batch
  // prefetch may run ahead of replay (extensibility.md §4.2.1); never consulted by enforcement.
  headroom(): bigint | undefined {
    const stmt = this.limit > 0n ? this.limit - this.accrued : undefined;
    const l = this.lifetime;
    const life = l !== undefined && l.limit > 0n ? l.limit - l.total : undefined;
    if (stmt !== undefined && life !== undefined) return stmt < life ? stmt : life;
    return stmt ?? life;
  }

  // isUnmetered reports whether NO cost ceiling is armed — no per-statement maxCost and no session
  // lifetime budget — so guard() is a no-op. The gate for the Track A2/A3 columnar fast path
  // (packed-leaf.md §11): it charges the scan block in bulk (not per row with an intervening guard),
  // which reproduces the row path's exact total only when there is nothing to abort against; a metered
  // query keeps the row path so its deterministic 54P01/54P02 abort row is unchanged. (Cancellation is
  // boundary-only in the TS core — an AbortSignal checked at the cursor yield, not a per-guard poll — so
  // it never moves the deterministic cost abort and is not part of this gate.)
  isUnmetered(): boolean {
    return this.limit <= 0n && (this.lifetime === undefined || this.lifetime.limit <= 0n);
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
    if (this.ceilingHit !== 0) pickLife = this.ceilingHit === 2;
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

function saturatingCostAdd(a: bigint, b: bigint): bigint {
  const n = a + b;
  return n > 9223372036854775807n ? 9223372036854775807n : n;
}
