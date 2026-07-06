import type { ActiveTx, SessionOptions, TxStatus } from "./snapshot.ts";
import { LifetimeBudget, Meter } from "./cost.ts";
import { Seam } from "./seam.ts";
import type { SequenceDef } from "./catalog.ts";
import { Privileges } from "./privileges.ts";
import { Snapshot, requireCustomVarName, txStatusOf } from "./snapshot.ts";
import type { ZoneRef } from "./timezone.ts";
import {
  DEFAULT_MAX_SQL_LENGTH,
  DEFAULT_TEMP_BUFFERS,
  Engine,
  distinctRowKey,
  finalizeAcc,
  foldAcc,
  needsEagerScan,
  newAccFromSpec,
} from "./executor.ts";
import { DEFAULT_WORK_MEM } from "./spill.ts";
import { resolveZone } from "./timezone.ts";
import { engineError } from "./errors.ts";
import type { PrivilegeSet } from "./privileges.ts";
import type { ClockFunc, RandomFill } from "./seam.ts";
import type { Acc, AggSpec, EvalEnv, RExpr, ResolvedFrame, SelectPlan } from "./executor.ts";
import type { Value } from "./value.ts";
import type { Row } from "./storage.ts";
import { isTrue, nullValue } from "./value.ts";
import { evalExpr } from "./eval.ts";
import { COSTS } from "./costs.ts";
import { TableStore } from "./storage.ts";
import type { KeyBound } from "./pmap.ts";
export class SessionState {
  // The open transaction, or null under autocommit (transactions.md §4.1); the Idle/Open/Failed
  // status (session.md §2.2) is derived from this.
  tx: ActiveTx | null;
  // The execution-cost ceiling (CLAUDE.md §13; api.md §8), or 0n for unlimited. Bounds every statement
  // run on this session: its Meter aborts 54P01 the instant accrued cost reaches it.
  maxCost: bigint;
  // The per-session cumulative cost budget (spec/design/session.md §5.4) and the session's running
  // CUMULATIVE cost, held together in a LifetimeBudget object shared (by reference) with every
  // statement Meter, which live-charges into it — so partial cost of an aborted statement counts and
  // the cumulative survives the swap (TS swaps the session object by reference). SESSION state, not
  // snapshot state: the cumulative does NOT roll back with a transaction. The budget is 0n ⇒ unlimited
  // (track-only); a statement aborts 54P02 the instant lifetime.total reaches lifetime.limit.
  lifetime: LifetimeBudget;
  // The maximum input SQL length in bytes (CLAUDE.md §13; cost.md §7); 0 = unlimited; default
  // DEFAULT_MAX_SQL_LENGTH. Over-limit input is rejected 54000 at parse, before lexing.
  maxSqlLength: number;
  // The work-memory budget in bytes (spec/design/spill.md §2): the memory a blocking operator holds
  // before it spills. 0 = unlimited; default DEFAULT_WORK_MEM. Never changes what a query observes.
  workMem: number;
  // The entropy + clock seam for the uuid generators / clock functions (entropy.md): two
  // host-injectable functions, each unset ⇒ the platform primitive. Tests inject seededRandomSource +
  // fixedClock (the # seed: / # clock: directives) for byte-identical cross-core output.
  seam: Seam;
  // SESSION currval state (sequences.md §6): the last value nextval/setval(…,true) produced IN THIS
  // SESSION for each sequence (lowercased name). NOT in the snapshot, NOT persisted.
  sessionSeq: Map<string, bigint>;
  // SESSION lastval state (sequences.md §6): the lowercased name of the sequence the most recent
  // nextval ran on — null before the first nextval.
  sessionLastName: string | null;
  // Per-STATEMENT running sequence advances (sequences.md §4); flushed into the working snapshot on
  // success, discarded on error (the transactional rollback of the advance, §5).
  pendingSeq: Map<string, SequenceDef>;
  // Per-STATEMENT running currval updates → flushed into sessionSeq on success.
  pendingCurrval: Map<string, bigint>;
  // Per-STATEMENT running lastval update → flushed into sessionLastName on success.
  pendingLastName: string | null;
  // The authorization envelope (spec/design/session.md §5.3): the GRANT/REVOKE-style per-object
  // privilege model the host configures and the engine enforces (42501) at name resolution. A fresh
  // session is fully permissive (every table privilege, every function EXECUTE).
  privileges: Privileges;
  // Whether PERSISTENT DDL (CREATE/DROP/ALTER of persistent relations) is permitted on this session
  // (§5.3); a denied schema change is 42501. Default on. Its scope narrows with temporary tables
  // (temp-tables.md §5): allowTempDdl is the temp-scoped sibling gate.
  allowDdl: boolean;
  // Whether session-local TEMPORARY-table DDL is permitted (spec/design/temp-tables.md §5); a denied
  // temp DDL is 42501. Resolved at construction from opts.allowTempDdl (defaulting to allowDdl's value).
  allowTempDdl: boolean;
  // The per-session temp-table storage budget in BYTES (temp-tables.md §7); 0 ⇒ unlimited. An
  // over-budget temp write aborts 54P03.
  tempBuffers: number;
  // The session-local TEMPORARY-table catalog + stores (spec/design/temp-tables.md §2): a Snapshot
  // holding only this session's temp tables, their stores, and their (UNIQUE) index stores. NEVER
  // serialized — only Engine.committed is written to the file, so a temp table makes ZERO file
  // writes. Private to this Session (it carries across the by-reference session swap and is invisible
  // to other sessions), dropped wholesale with the session. Transactional like the main snapshot: an
  // open transaction clones it into ActiveTx.tempWorking, adopted on a successful COMMIT, discarded on
  // ROLLBACK.
  tempCommitted: Snapshot;
  // The session variables (spec/design/session.md §6.1): PostgreSQL's GUC model scoped to the session
  // — a string→string map (PG GUCs are all text) the host sets (setVar/resetVar) and SQL reads with
  // current_setting. Custom (dotted) names only in v1. SESSION state, not snapshot state: it does NOT
  // roll back with a transaction (PG SET SESSION), and each session keeps its own map across the
  // by-reference swap (like the privilege envelope).
  vars: Map<string, string>;
  // The resolved session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone a
  // timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts. Resolved once
  // (from opts.timeZone at construction, or setTimeZone) to a cheap ZoneRef (UTC = fixed 0); the
  // evaluator reads it via the active session. SESSION state (no storage effect).
  timeZone: ZoneRef;
  // The read pin for a data-modifying WITH statement (spec/design/writable-cte.md §2): the single
  // pre-statement snapshot every sub-statement reads, so the data-modifying CTEs and the primary
  // cannot observe each other's table writes (their writes still accumulate into the transaction's
  // working). Set by the writable-CTE orchestrator before the first sub-statement runs and cleared
  // when it finishes (success or error); null for every other statement, where reads fall through to
  // working/committed as usual (readSnap).
  readPin: Snapshot | null;

  constructor(opts: SessionOptions = {}) {
    this.tx = null;
    this.maxCost = opts.maxCost ?? 0n;
    this.lifetime = new LifetimeBudget(opts.lifetimeMaxCost ?? 0n);
    this.maxSqlLength = opts.maxSqlLength ?? DEFAULT_MAX_SQL_LENGTH;
    // 0 (or unset) ⇒ the default budget, not unlimited — the zero value stays a safe finite budget
    // (unlike maxCost/lifetimeMaxCost, whose default genuinely is 0 ⇒ unlimited). Unbounded/never-spill
    // is reached at runtime via setWorkMem(0). Matches Go/Rust (api.md §2.1).
    this.workMem = opts.workMem ? opts.workMem : DEFAULT_WORK_MEM;
    this.seam = new Seam();
    this.sessionSeq = new Map();
    this.sessionLastName = null;
    this.pendingSeq = new Map();
    this.pendingCurrval = new Map();
    this.pendingLastName = null;
    this.privileges = new Privileges();
    if (opts.defaultPrivileges !== undefined) {
      this.privileges.setDefaultTable(opts.defaultPrivileges);
    }
    this.allowDdl = opts.allowDdl ?? true;
    // Back-compat default-inheritance (temp-tables.md §5): an unset allowTempDdl takes allowDdl's
    // value, so a session configured before temp tables existed behaves as before.
    this.allowTempDdl = opts.allowTempDdl ?? this.allowDdl;
    this.tempBuffers = opts.tempBuffers ?? DEFAULT_TEMP_BUFFERS;
    this.tempCommitted = new Snapshot();
    this.vars = new Map();
    // Resolve the configured zone once; an invalid value falls back to UTC at construction (the
    // validated path is setTimeZone, which surfaces 22023). timezones.md §9.4.
    this.timeZone = resolveZone(opts.timeZone ?? "UTC") ?? { fixed: true, off: 0 };
    this.readPin = null;
  }

  // setTimeZone sets the session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone
  // a timestamptz is decomposed in. Accepts UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded
  // JTZ bundle provides; a name no bundle provides (and not a built-in) is 22023, the value unchanged.
  setTimeZone(zone: string): void {
    const zr = resolveZone(zone);
    if (zr === undefined) {
      throw engineError("invalid_parameter_value", `time zone "${zone}" not recognized`);
    }
    this.timeZone = zr;
  }

  // status is this session's transaction status (Idle/Open/Failed, session.md §2.2).
  status(): TxStatus {
    return txStatusOf(this.tx);
  }
  // inTransaction reports whether an explicit transaction block is open on this session.
  inTransaction(): boolean {
    return this.tx !== null;
  }
  setMaxCost(limit: bigint): void {
    this.maxCost = limit;
  }
  // setLifetimeMaxCost sets the per-session cumulative cost budget (spec/design/session.md §5.4);
  // <= 0n ⇒ unlimited. A statement aborts 54P02 the instant the session's cumulative cost reaches it,
  // and once spent every further statement is rejected 54P02 at admission.
  setLifetimeMaxCost(limit: bigint): void {
    this.lifetime.limit = limit;
  }
  // lifetimeMaxCost is the current per-session cumulative cost budget (0n ⇒ unlimited).
  lifetimeMaxCost(): bigint {
    return this.lifetime.limit;
  }
  // lifetimeCost is the session's running CUMULATIVE execution cost so far (spec/design/session.md
  // §5.4) — the gauge the budget bounds. Tracked even when unlimited; survives a transaction rollback.
  lifetimeCost(): bigint {
    return this.lifetime.total;
  }
  // newMeter builds the Meter for a statement run on this session: the per-statement maxCost ceiling
  // (54P01) plus the shared LifetimeBudget (54P02) the meter live-charges into. Every statement's
  // meter is minted here, so all execution cost accrues into the cumulative.
  newMeter(): Meter {
    return new Meter(this.maxCost, this.lifetime);
  }
  setMaxSqlLength(bytes: number): void {
    this.maxSqlLength = bytes;
  }
  setWorkMem(bytes: number): void {
    this.workMem = bytes;
  }
  // setDefaultPrivileges replaces the default table-privilege set — the GRANT … ON ALL TABLES default
  // (§5.3). A read-only session is PrivilegeSet.empty().with("select").
  setDefaultPrivileges(privs: PrivilegeSet): void {
    this.privileges.setDefaultTable(privs);
  }
  // grant grants privs on a specific object (table or function), beyond the default (§5.3).
  grant(privs: PrivilegeSet, object: string): void {
    this.privileges.grant(privs, object);
  }
  // revoke revokes privs from a specific object (revoke wins over grant and the default, §5.3).
  revoke(privs: PrivilegeSet, object: string): void {
    this.privileges.revoke(privs, object);
  }
  // setAllowDdl sets whether DDL is permitted on this session (§5.3); a denied change is 42501.
  setAllowDdl(allow: boolean): void {
    this.allowDdl = allow;
  }
  // setAllowTempDdl sets whether session-local temporary-table DDL is permitted (temp-tables.md §5).
  setAllowTempDdl(allow: boolean): void {
    this.allowTempDdl = allow;
  }
  // setTempBuffers sets the per-session temp-table storage budget in BYTES (temp-tables.md §7); 0 ⇒
  // unlimited. An over-budget temp write aborts 54P03.
  setTempBuffers(bytes: number): void {
    this.tempBuffers = bytes;
  }
  // setVar sets a session variable (spec/design/session.md §6.1) — PostgreSQL's GUC model, scoped to
  // the session. Custom variables must be namespaced (a dotted name like myapp.tenant); a non-dotted
  // name is 42704 (no built-in setting is reachable through this map in v1 — the time_zone built-in is
  // its own slice). The name is case-insensitive (folded to lowercase, PG); the value is text. Session
  // state, not snapshot state — it does NOT roll back with a transaction.
  setVar(name: string, value: string): void {
    this.vars.set(requireCustomVarName(name), value);
  }
  // resetVar clears a session variable (§6.1). A non-dotted name is 42704 (as for setVar); an unset
  // name is a no-op (PG RESET of an unset custom variable).
  resetVar(name: string): void {
    this.vars.delete(requireCustomVarName(name));
  }
  // var reads a session variable's value (§6.1), or undefined if it is not set. The host getter never
  // throws — it is the SQL current_setting read that raises 42704 on an unset name.
  var(name: string): string | undefined {
    return this.vars.get(name.toLowerCase());
  }
  // resetVars clears every session variable (§6.1) — PostgreSQL's RESET ALL for the variable map (also
  // the per-record reset hook the conformance harness's # set: directive uses).
  resetVars(): void {
    this.vars.clear();
  }
  setRandomSource(f: RandomFill): void {
    this.seam.randomFill = f;
  }
  clearRandomSource(): void {
    this.seam.randomFill = undefined;
  }
  setClockSource(f: ClockFunc): void {
    this.seam.clock = f;
  }
  clearClockSource(): void {
    this.seam.clock = undefined;
  }
}

// streamingScanEligible reports whether plan is the single-table, no-blocking-operator STREAMING SCAN
// shape (spec/design/cost.md §3, streaming.md §4) — a single relation, no join/aggregate/window, an
// output order the primary-key scan already yields (pkOrdered, or no ORDER BY with a LIMIT
// short-circuit), no index/GIN/GiST bound (those read the full admitted set eagerly), and a real table
// store (not an SRF / CTE / derived source). Both execSelectPlan (which routes to the eager
// execStreamingScan) and tryStreamingQuery (the lazy query() lane) gate on this ONE predicate, so the
// two never drift.
export function streamingScanEligible(plan: SelectPlan): boolean {
  return (
    plan.rels.length === 1 &&
    plan.joins.length === 0 &&
    !plan.isAgg &&
    !plan.hasWindow &&
    (plan.pkOrdered || (!plan.distinct && plan.order.length === 0 && plan.limit !== null)) &&
    !needsEagerScan(plan.relBounds[0]) &&
    plan.rels[0]!.srf === undefined &&
    plan.rels[0]!.cte === undefined &&
    plan.rels[0]!.derived === undefined
  );
}

// frameBackwardSafe reports whether a frame folds only rows at or before the current row in the scan
// order (spec/design/window.md §5.2/§6). The frame END must not look forward; a RANGE/GROUPS
// CURRENT-ROW end spans the current peer group, which pulls in later rows unless the ordering key is
// unique. A ROWS frame uses physical position, so it never expands to peers. The default frame
// (undefined/null, with a window ORDER BY) is RANGE UNBOUNDED PRECEDING TO CURRENT ROW — safe only
// when the key is unique.
export function frameBackwardSafe(
  frame: ResolvedFrame | null | undefined,
  unique: boolean,
): boolean {
  if (frame === null || frame === undefined) return unique;
  switch (frame.end.kind) {
    case "unboundedPreceding":
    case "preceding":
      return true; // strictly before the current peer group
    case "currentRow":
      return frame.mode === "rows" || unique; // ROWS = the physical row; RANGE/GROUPS = the peer group
    default:
      return false; // following / unboundedFollowing look forward
  }
}

// vectorizedProjectEligible reports whether plan is a shape projectColumnar specializes: a bare-column
// projection over a single base table with no join / aggregate / window / DISTINCT / ORDER BY / LIMIT /
// OFFSET and no index/GIN/GiST bound — a plain `SELECT c0, c3, … FROM t [WHERE …]` whose output is the
// (optionally filtered) scan-order rows narrowed to a column subset. A residual filter is allowed (A3):
// projectColumnar applies it over the lanes into a selection vector. Pure plan inspection (charges
// nothing), so a bail is free and the general materialize path runs with identical results + cost; the
// store / paging / spillable / column-range gates live in projectColumnar, which declines to that path.
// LIMIT/OFFSET is excluded deliberately: a LIMIT with no ORDER BY streams with an early exit
// (streamingScanEligible), which the whole-table gather must not steal.
export function vectorizedProjectEligible(plan: SelectPlan): boolean {
  if (plan.isAgg || plan.hasWindow || plan.distinct) return false;
  if (plan.rels.length !== 1 || plan.joins.length !== 0) return false;
  const rel = plan.rels[0]!;
  if (
    rel.srf !== undefined ||
    rel.cte !== undefined ||
    rel.derived !== undefined ||
    rel.lateral === true
  ) {
    return false;
  }
  // No ORDER BY / LIMIT / OFFSET (those route to a streaming / sort / index path). A residual filter is
  // fine — projectColumnar vectorizes it (A3).
  if (plan.order.length !== 0 || plan.limit !== null || plan.offset !== null) return false;
  // Full scan or a primary-key bound only — an index / GIN / GiST / point-set bound changes the scan
  // mechanics (needsEagerScan), so it keeps the general materialize path.
  if (needsEagerScan(plan.relBounds[0])) return false;
  // Every projection must be a bare column reference: a bare "column" evaluates to row[index] with zero
  // operator_eval, so gathering it from a dense lane is cost-identical. An expression projection
  // (`c0 + 1`, a function call) charges operator_eval and needs a row — it keeps the row path.
  if (plan.projections.length === 0) return false;
  return plan.projections.every((p) => p.kind === "column");
}

// filterColumnar evaluates filter over the gathered per-column lanes and returns the surviving row
// indices (the selection vector) — filter vectorization (packed-leaf.md §11 Track A3). It reuses the
// scalar evalExpr verbatim over a SINGLE reusable scratch row (the masked columns filled from the lanes
// at that row index, untouched columns left NULL), so the predicate's operator_eval charges and its 3VL
// survivor test (keep iff TRUE) are byte-identical to the scalar WHERE loop — and the result is identical
// too, because the row path also feeds the filter a MASKED row (untouched columns NULL via
// resolveColumns / rowAtMasked) and the filter references only masked columns (collectTouched includes
// the filter), so a scratch row filled from the lanes is the same input. The one reusable scratch row is
// the allocation win: no full-width row per scanned row, only the survivor indices. The caller has
// verified no touched column spills, so every masked lane is a non-empty Value[] of length rowCount (an
// untouched column's lane stays empty but is never read).
export function filterColumnar(
  filter: RExpr,
  cols: Value[][],
  mask: boolean[],
  rowCount: number,
  env: EvalEnv,
  meter: Meter,
): number[] {
  const sel: number[] = [];
  const scratch: Row = new Array(mask.length).fill(nullValue());
  for (let i = 0; i < rowCount; i++) {
    for (let c = 0; c < mask.length; c++) {
      if (mask[c]) scratch[c] = cols[c]![i]!;
    }
    if (isTrue(evalExpr(filter, scratch, env, meter))) sel.push(i);
  }
  return sel;
}

// vectorizedSpecEligible reports whether one aggregate is a specialized numeric kernel the vectorized
// aggregate path folds: a plain (non-DISTINCT, non-FILTER, non-ordered-set, non-hypothetical) COUNT(*)
// / COUNT(col) / SUM(i16|i32) / SUM|AVG(f32|f64) / MIN(col) / MAX(col) whose operand (where it has one)
// is a bare column reference. SUM(i64|decimal) → "sumDecimal" and AVG(decimal) → "avg" are deferred
// (their fold charges running-sum-dependent decimalWork); MIN/MAX fold ANY type through valueCmp. The
// ordered-set / hypothetical / json plans are excluded by the switch's default; reusing the shared
// foldAcc keeps the fold byte-identical to the scalar path (the scalar grouped path folds through it).
export function vectorizedSpecEligible(spec: AggSpec): boolean {
  if (spec.distinct === true) return false;
  if (spec.filter !== undefined && spec.filter !== null) return false;
  switch (spec.plan) {
    case "countStar":
      return spec.operand === null;
    case "count":
    case "sumInt":
    case "sumFloat":
    case "avgFloat":
    case "min":
    case "max":
      return spec.operand !== null && spec.operand.kind === "column";
    default:
      return false;
  }
}

// operandCol is the bare-column ordinal an eligible aggregate reads (its operand `{kind:"column"}`), or
// null for COUNT(*) (which folds no value). Eligibility (vectorizedSpecEligible) guarantees the operand
// is either absent or a bare column, so this is total over an eligible spec.
export function operandCol(spec: AggSpec): number | null {
  return spec.operand !== null && spec.operand.kind === "column" ? spec.operand.index : null;
}

// LaneAt is the survivor value source for the vectorized fold — the ONE seam that differs between the
// row path (a Row[] of full rows) and the columnar path (dense per-column lanes + an optional A3
// selection vector). `at(j, col)` reads survivor j's value in column col, so the fold kernels below are
// written once and run either way. Cost is unaffected: both feed the same values in scan order.
export type LaneAt = (j: number, col: number) => Value;

// foldAggWhole folds one WHOLE-TABLE grand-total group over `nsurv` survivors from `at`, returning the
// finalized aggregate results [agg0, …] (the synthetic row for a () group — no key columns). It builds
// one Acc per spec and folds each survivor's operand value through the shared foldAcc (identical acc
// state, hence finalizeAcc, to the scalar path), charging aggregateAccumulate once per (survivor × spec)
// in bulk — the identical total to the scalar loop (per row × spec), cost-safe because the caller gates
// to the unmetered lane (no per-row guard to preserve).
export function foldAggWhole(specs: AggSpec[], at: LaneAt, nsurv: number, meter: Meter): Value[] {
  const accs = specs.map((s) => newAccFromSpec(s));
  specs.forEach((spec, si) => {
    meter.charge(COSTS.aggregateAccumulate * BigInt(nsurv));
    const oc = operandCol(spec);
    for (let j = 0; j < nsurv; j++) {
      foldAcc(accs[si]!, oc === null ? nullValue() : at(j, oc), meter);
    }
  });
  return accs.map((a) => finalizeAcc(a));
}

// groupByIntKey buckets `nsurv` survivors from `at` by their single INTEGER group-key column and folds
// each aggregate per group, returning the finalized synthetic rows [key, agg0, …] in scan-order-of-
// first-appearance. The bucket is a Map<bigint, number> over the raw key (a bijection of the scalar
// path's value-canonical group key for a fixed-width integer column) plus one sentinel group for NULL
// keys. The fold reuses foldAcc (byte-identical acc state); aggregateAccumulate is charged once per
// (survivor × spec) in bulk — the identical total to the scalar loop. The bucketing is unmetered
// (cost.md §3), so the bigint map is a free internal choice. The caller has verified every needed lane
// is populated.
export function groupByIntKey(
  specs: AggSpec[],
  keyCol: number,
  at: LaneAt,
  nsurv: number,
  meter: Meter,
): Value[][] {
  const groups: { key: Value; accs: Acc[] }[] = [];
  const index = new Map<bigint, number>();
  let nullGi = -1;

  meter.charge(COSTS.aggregateAccumulate * BigInt(nsurv) * BigInt(specs.length));
  for (let j = 0; j < nsurv; j++) {
    const kv = at(j, keyCol);
    let gi: number;
    if (kv.kind === "int") {
      const g = index.get(kv.int);
      if (g === undefined) {
        gi = groups.length;
        index.set(kv.int, gi);
        groups.push({ key: kv, accs: specs.map((s) => newAccFromSpec(s)) });
      } else {
        gi = g;
      }
    } else {
      // A NULL integer key (the only other case for an integer column) buckets into one sentinel
      // group, exactly as the scalar path groups all NULLs together.
      if (nullGi < 0) {
        nullGi = groups.length;
        groups.push({ key: nullValue(), accs: specs.map((s) => newAccFromSpec(s)) });
      }
      gi = nullGi;
    }
    const accs = groups[gi]!.accs;
    specs.forEach((spec, si) => {
      const oc = operandCol(spec);
      foldAcc(accs[si]!, oc === null ? nullValue() : at(j, oc), meter);
    });
  }

  return groups.map((g) => {
    const srow: Value[] = [g.key];
    for (const a of g.accs) srow.push(finalizeAcc(a));
    return srow;
  });
}

// streamRows is the lazy pull pipeline behind a streaming cursor (spec/design/streaming.md §3/§4, S3):
// execStreamingScan's per-row loop as a function* generator (the natural pull form in JS). It scans the
// snapshot store, resolves touched columns, applies WHERE, and projects — YIELDING ONE output row at a
// time, accruing the identical cost units at the identical sites as the eager path. So a fully-drained
// streaming query observes the same rows + total cost (streaming.md §6), while a caller that stops early
// abandons the generator (its for-of returns the inner scanIter) and faults no further leaves. The
// LIMIT check sits AFTER the yield, so once the window is full the generator returns BEFORE the for-of
// pulls another row (matching execStreamingScan's stop-after-the-limit-th-row, cost.md §3).
export function* streamRows(
  sp: SelectPlan,
  env: EvalEnv,
  meter: Meter,
  store: TableStore,
  bound: KeyBound,
  empty: boolean,
): Generator<Value[]> {
  if (empty || sp.limit === 0n) return;
  const offset = sp.offset ?? 0n;
  const distinct = sp.distinct;
  const seen = new Set<string>();
  let passed = 0n;
  let produced = 0n;
  // A pkReverse plan (ORDER BY the full PK all-DESC) walks the tree backward; everything else forward.
  for (const [, rawRow] of store.scanIter(bound, sp.pkReverse)) {
    meter.guard(); // enforce the cost ceiling per scanned row (CLAUDE.md §13)
    meter.charge(COSTS.storageRowRead);
    // Materialize the touched columns left unfetched by the lazy load (large-values.md §14); the chain
    // reads were already metered in the up-front block (cost.md §3).
    const row = store.resolveColumns(rawRow, sp.relMasks[0]!);
    if (sp.filter !== null && !isTrue(evalExpr(sp.filter, row, env, meter))) continue;
    if (distinct) {
      // DISTINCT (cost.md §3): project EVERY scanned filtered row (the dedup key, charged even for a
      // duplicate — the §3 asymmetry), drop a value already seen, then OFFSET/LIMIT window the survivors.
      const tuple = sp.projections.map((p) => evalExpr(p, row, env, meter));
      const key = distinctRowKey(tuple);
      if (seen.has(key)) continue;
      seen.add(key);
      passed += 1n;
      if (passed <= offset) continue;
      meter.charge(COSTS.rowProduced);
      produced += 1n;
      yield tuple;
    } else {
      passed += 1n;
      if (passed <= offset) continue;
      meter.charge(COSTS.rowProduced);
      produced += 1n;
      yield sp.projections.map((p) => evalExpr(p, row, env, meter));
    }
    // The LIMIT short-circuit (cost.md §3): once the window is full, stop WITHOUT pulling another row —
    // so no further leaf is faulted (the streaming early-exit win). The check is after the yield, so the
    // for-of pulls the next row only when another is actually needed.
    if (sp.limit !== null && produced >= sp.limit) return;
  }
}

// bufferedRows is the lazy pull pipeline behind a BUFFERED cursor for a blocking plan
// (spec/design/streaming.md §4, S4): a function* generator whose body runs the BLOCKING part
// (engine.execSelectEmit) on its first .next() — buffering the input (correctly: a sort/group/dedup/
// join must see it all) and charging the scan/sort/group/dedup cost — then YIELDS its buffer one row at
// a time. A "project" row is projected (charging rowProduced + projection) on emission; a "sorted" row
// is pulled from the SortedRows iterator and projected (the streaming-sort output, streaming.md §4/§7);
// an "identity"/"final" row is handed out (already projected). So building the generator runs no work (a 54P01 cost
// abort surfaces during the first pull, not at query() — streaming.md §6), peak output memory is one
// row, a caller's early exit (abandoning the generator) skips the projection of the rows it never pulls,
// and a fully-drained query observes the same rows + total cost as the eager path (streaming.md §6).
export function* bufferedRows(
  engine: Engine,
  plan: SelectPlan,
  env: EvalEnv,
  meter: Meter,
  params: Value[],
): Generator<Value[]> {
  const em = engine.execSelectEmit(plan, env, meter, params);
  if (em.mode === "final") {
    // Already projected + charged — hand each row out (no further cost).
    for (const row of em.rows) yield row;
    return;
  }
  if (em.mode === "sorted") {
    // The streaming sort's lazy output: pull the next windowed row from the SortedRows iterator, charge
    // rowProduced, and project it (streaming.md §4/§7). The try/finally releases any undrained spill runs
    // when the generator is returned early (a caller's early exit) or completes (§5).
    const sorted = em.sorted!;
    try {
      for (let i = 0; i < em.end; i++) {
        const row = sorted.next();
        if (row === null) break;
        meter.guard(); // enforce the cost ceiling / cancellation per produced row (CLAUDE.md §13)
        meter.charge(COSTS.rowProduced);
        yield plan.projections.map((p) => evalExpr(p, row, env, meter));
      }
    } finally {
      sorted.close();
    }
    return;
  }
  if (em.mode === "columnar") {
    // Columnar projection (packed-leaf.md §11 Track A2/A3): gather each row from the dense lanes — a
    // bare-column projection with no full-width row — charging only rowProduced (a bare column ref is a
    // zero-cost slot read). A set `sel` (the A3 filter's survivors) maps output row j to lane position
    // sel[j]. An early exit skips the rowProduced of the rows it never pulls.
    const cols = em.cols!;
    const projCols = em.projCols!;
    const sel = em.sel;
    for (let j = em.start; j < em.end; j++) {
      meter.guard(); // enforce the cost ceiling / cancellation per produced row (CLAUDE.md §13)
      meter.charge(COSTS.rowProduced);
      const l = sel === undefined ? j : sel[j]!;
      yield projCols.map((c) => cols[c]![l]!);
    }
    return;
  }
  for (let i = em.start; i < em.end; i++) {
    meter.guard(); // enforce the cost ceiling / cancellation per produced row (CLAUDE.md §13)
    meter.charge(COSTS.rowProduced);
    if (em.mode === "identity") yield em.rows[i]!;
    else yield plan.projections.map((p) => evalExpr(p, em.rows[i]!, env, meter));
  }
}

// Attachment is one host-attached DATABASE-scoped database in a handle's namespace
// (spec/design/attached-databases.md §2): a named (storage, mode) pair reachable by a database
// qualifier. Its MUTABLE storage identity (page accounting, block store + pager) lives here — an
// Engine over an in-RAM MemoryBlockStore, exactly like the temp domain (newAttachedStorage). The
// immutable committed snapshot lives in the core's attached roots under the same key, so a reader pins
// it lock-free together with every other root. An attachment is file-backed (Slice 2 — storage.path is
// non-null, a FileBlockStore behind the pager, committed durably via commitDurableAttachment) or
// in-memory (storage.path is null, a MemoryBlockStore, committed via persistTemp). The storage Engine's
// path is the sole source of the file/memory distinction.
export type Attachment = {
  name: string; // lowercased qualifier name (the registry key)
  readOnly: boolean; // a read-only attachment rejects every write (DML + DDL) with 25006 (§4)
  storage: Engine; // the block store (file or in-memory) + pager + page accounting
};

// AttachmentCore is the minimal view of the shared core the executor needs for attachment routing
// (spec/design/attached-databases.md §5): the core-owned registry, the reader-liveness watermark, and
// whether MAIN is durable (the one-durable-writer count, §5). SharedCore (shared.ts) implements it
// structurally; a bare/transient engine carries `core = null`. Declared here (not imported from
// shared.ts) because shared.ts imports the Engine, not the reverse.
export interface AttachmentCore {
  attachments: Map<string, Attachment>;
  hasLiveReaders(): boolean;
  mainIsDurable(): boolean;
}

// isReservedScope reports whether a database qualifier names one of the two implicit reserved scopes
// `main` / `temp` (attached-databases.md §3), which resolve to the SAME store the bare name would — so
// a qualified reference to one keeps every existing fast path. An undefined qualifier (a bare
// implicit-scope name) counts as reserved for routing: it too keeps the temp-first funnels.
export function isReservedScope(q: string | undefined): boolean {
  if (q === undefined) return true;
  const l = q.toLowerCase();
  return l === "main" || l === "temp";
}

// isAttachmentScope reports whether a database qualifier names a HOST-ATTACHED database (not undefined,
// not reserved main/temp) — the case that routes to the attachment registry rather than the implicit
// temp-first funnels, and the case that gates off index-bound pushdown this slice (attached-databases.md §8).
export function isAttachmentScope(q: string | undefined): boolean {
  return !isReservedScope(q);
}
