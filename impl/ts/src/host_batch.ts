// Host-function batch prefetch (spec/design/extensibility.md §4.2.1) — speculative batch, scalar
// replay. An evaluation site that already holds a chunk of rows asks HostBatch.setRow to call each
// eligible host function ONCE over the chunk's argument columns, then replays its ordinary
// row-at-a-time evaluation with the batch installed on the EvalEnv (env.hostBatch). The hostFunc eval
// case still charges, guards, and type-checks per row exactly as before, and evaluates its arguments
// and short-circuits NULL for any row the batch cannot answer; for a prefetched row it takes the
// kernel's outcome from the cache, skipping the argument evaluation that would charge nothing, raise
// nothing, and not short-circuit. So rows, cost, abort points, and errors are identical to
// batch-of-one by construction.
//
// A site that holds its rows (the "project" buffer) prefetches from them directly. A site that PULLS
// its rows (the streaming scan, the streaming sort / spool) first fills a ReadAhead window from its
// source — pulling is not a metered event there, and a source error is deferred to the row where the
// batch-of-one pull would have raised it (§4.2.1 "In-hand and pulled sites"). Mirrors
// impl/rust/src/executor/host_batch.rs.

import { Meter } from "./cost.ts";
import { evalExpr } from "./eval.ts";
import type { EvalEnv, RExpr } from "./executor.ts";
import type { ExtensionRegistry, HostOutcome } from "./extension.ts";
import type { Row } from "./storage.ts";
import type { Value } from "./value.ts";

// The largest chunk a site prefetches (extensibility.md §4.2.1).
export const HOST_BATCH_ROWS = 1024;

// The largest read-ahead window a pulled site fills (extensibility.md §4.2.1): every pulled row is a
// fresh allocation the window keeps alive only to batch, so the window stays small — still 64× fewer
// kernel crossings.
export const READ_AHEAD_ROWS = 64;

type HostFuncNode = Extract<RExpr, { kind: "hostFunc" }>;

// One eligible call node's prefetched outcomes over the site's current chunk.
type Slot = {
  // The call node itself (in the site's expression tree) — compared by identity. A node outside this
  // site's expression list (a subquery's) never matches.
  node: HostFuncNode;
  // The site row index of outcomes[0].
  base: number;
  // How many chunk rows the prefetch settled: the whole chunk, or through the row the kernel reported
  // failing. A row at or past base + valid starts a new chunk.
  valid: number;
  // Per chunk row: the kernel's outcome, or undefined (not prefetched — a NULL / unfetched argument,
  // or a row past a reported error). Taken (cleared) on replay.
  outcomes: (HostOutcome | undefined)[];
};

// Whether `e` is free and cannot raise, so a prefetch may evaluate it without a meter: a column or
// outer-column reference, a parameter, or a scalar constant (not a row/array/range constant).
function triviallyEvaluable(e: RExpr): boolean {
  switch (e.kind) {
    case "column":
    case "outerColumn":
    case "param":
    case "constInt":
    case "constFloat":
    case "constBool":
    case "constText":
    case "constDecimal":
    case "constBytea":
    case "constUuid":
    case "constTimestamp":
    case "constTimestamptz":
    case "constDate":
    case "constInterval":
    case "constJson":
    case "constJsonb":
    case "constJsonPath":
    case "constNull":
      return true;
    default:
      return false;
  }
}

// Whether a column argument's stored value is still unfetched (a deferred large value,
// large-values.md §14): resolving it is engine I/O that can raise, so the prefetch leaves that row to
// replay (extensibility.md §4.2.1 rule 2).
function unfetchedArg(a: RExpr, row: Row, env: EvalEnv): boolean {
  if (a.kind === "column") return row[a.index]!.kind === "unfetched";
  if (a.kind === "outerColumn") {
    return env.outer[env.outer.length - a.level]![a.index]!.kind === "unfetched";
  }
  return false;
}

// Collect the eligible call nodes reachable from `e` (extensibility.md §4.2.1): a batch-kernel,
// non-volatile host call with at least one argument, all trivially evaluable, reached only through
// nodes that evaluate every operand (a cast, unary minus, NOT, IS [NOT] NULL, arithmetic, comparison).
function collect(e: RExpr, registry: ExtensionRegistry, out: Slot[]): void {
  switch (e.kind) {
    case "hostFunc":
      // A zero-argument call is never prefetched: its kernel receives no columns, so it could not
      // learn the batch's row count.
      if (
        e.args.length > 0 &&
        registry.functionAt(e.id).batchable() &&
        e.args.every(triviallyEvaluable)
      ) {
        out.push({ node: e, base: 0, valid: 0, outcomes: [] });
      }
      return;
    case "cast":
    case "neg":
    case "not":
    case "isNull":
      collect(e.operand, registry, out);
      return;
    case "arith":
    case "compare":
      collect(e.lhs, registry, out);
      collect(e.rhs, registry, out);
      return;
    default:
      return;
  }
}

// The prefetched host-function outcomes of one evaluation site (extensibility.md §4.2.1).
export class HostBatch {
  // The site's current row index, set by setRow before each row's evaluation.
  private row = 0;
  private readonly slots: Slot[];
  private readonly registry: ExtensionRegistry;

  private constructor(slots: Slot[], registry: ExtensionRegistry) {
    this.slots = slots;
    this.registry = registry;
  }

  // The batch for a site that evaluates every expression of `exprs` once per row, or null when no call
  // in them is eligible (§4.2.1).
  static forExprs(exprs: RExpr[], registry: ExtensionRegistry | null): HostBatch | null {
    if (registry === null) return null;
    const slots: Slot[] = [];
    for (const e of exprs) collect(e, registry, slots);
    return slots.length === 0 ? null : new HostBatch(slots, registry);
  }

  // Make row `i` of the site current. When a slot's chunk does not settle it, prefetch a new chunk
  // starting at `i` over rows[i..end) (at most HOST_BATCH_ROWS rows, further capped by the meter's
  // headroom).
  setRow(i: number, rows: Row[], end: number, env: EvalEnv, meter: Meter): void {
    this.row = i;
    const headroom = meter.headroom();
    for (const slot of this.slots) {
      if (i < slot.base || i >= slot.base + slot.valid)
        this.prefetch(slot, i, rows, end, env, headroom);
    }
  }

  // Forget every chunk — the site's row indexes restart (a new read-ahead window).
  reset(): void {
    for (const slot of this.slots) {
      slot.valid = 0;
      slot.outcomes = [];
    }
  }

  // The prefetched outcome of call node `node` for the current row, or undefined when the batch
  // cannot answer it (the caller then calls the kernel for this row alone).
  take(node: RExpr): HostOutcome | undefined {
    const slot = this.slots.find((s) => s.node === node);
    if (slot === undefined) return undefined;
    const i = this.row - slot.base;
    if (i < 0 || i >= slot.outcomes.length) return undefined;
    const outcome = slot.outcomes[i];
    slot.outcomes[i] = undefined;
    return outcome;
  }

  // Prefetch one slot's chunk starting at site row `start`.
  private prefetch(
    slot: Slot,
    start: number,
    rows: Row[],
    end: number,
    env: EvalEnv,
    headroom: bigint | undefined,
  ): void {
    const call = slot.node;
    const hf = this.registry.functionAt(call.id);
    let n = Math.min(end - start, HOST_BATCH_ROWS);
    // Every replayed row charges at least the declared cost, so the kernel never runs on more rows
    // than the remaining budget could pay for, plus the one that trips it (§4.2.1).
    if (headroom !== undefined && hf.cost > 0n) {
      const cap = (headroom > 0n ? headroom : 0n) / hf.cost + 1n;
      if (cap < BigInt(n)) n = Number(cap);
    }
    slot.base = start;
    slot.valid = n;
    slot.outcomes = new Array<HostOutcome | undefined>(n).fill(undefined);
    // Gather the argument columns over the chunk rows whose arguments are all non-NULL and fetched;
    // `members` maps each batch row back to its chunk row.
    const cols: Value[][] = call.args.map(() => []);
    const members: number[] = [];
    const scratch = new Meter();
    for (let k = 0; k < n; k++) {
      const row = rows[start + k]!;
      const vals: Value[] = [];
      let ok = true;
      for (const a of call.args) {
        if (unfetchedArg(a, row, env)) {
          ok = false;
          break;
        }
        let v: Value;
        try {
          v = evalExpr(a, row, env, scratch);
        } catch {
          ok = false;
          break;
        }
        if (v.kind === "null") {
          ok = false;
          break;
        }
        vals.push(v);
      }
      if (!ok) continue;
      for (let j = 0; j < vals.length; j++) cols[j]!.push(vals[j]!);
      members.push(k);
    }
    if (members.length === 0) return;
    const outcomes = hf.callBatch(cols, members.length);
    for (let m = 0; m < members.length; m++) {
      const outcome = outcomes[m];
      // The first member the kernel left unanswered follows a reported error: the chunk settles
      // through the error row, and a later row starts a new chunk.
      if (outcome === undefined) {
        slot.valid = members[m]!;
        break;
      }
      slot.outcomes[members[m]!] = outcome;
    }
  }
}

// A pulled site's read-ahead window (extensibility.md §4.2.1 "In-hand and pulled sites"): rows read
// ahead of replay from a source whose pull is not a metered event, plus the site's batches over them.
// The site hands rows out one at a time with next() and evaluates each through its unchanged per-row
// pipeline.
export class ReadAhead {
  // The current window, in source order.
  rows: Row[] = [];
  // The window index of the next row to hand out.
  private pos = 0;
  // A source error met while filling the window — raised once every row read before it has been
  // handed out (exactly where the batch-of-one pull would have raised it), dropped if replay stops
  // first. `hasDeferred` distinguishes a thrown undefined.
  private deferred: unknown = undefined;
  private hasDeferred = false;
  // The source reported its end.
  private ended = false;
  // The site's WHERE prefetch, if its predicate has an eligible call.
  readonly filter: HostBatch | null;
  // The site's projection prefetch, if a projection item has an eligible call.
  readonly project: HostBatch | null;

  private constructor(filter: HostBatch | null, project: HostBatch | null) {
    this.filter = filter;
    this.project = project;
  }

  // The window for a site evaluating `filter` (if any) and `project` per row, or null when neither has
  // an eligible call — the site then pulls row by row as before.
  static forSite(
    filter: RExpr | null,
    project: RExpr[],
    registry: ExtensionRegistry | null,
  ): ReadAhead | null {
    const f = filter === null ? null : HostBatch.forExprs([filter], registry);
    const p = HostBatch.forExprs(project, registry);
    return f === null && p === null ? null : new ReadAhead(f, p);
  }

  // Point the window at a new source (the next interval of an interval-set scan).
  restart(): void {
    this.rows = [];
    this.pos = 0;
    this.deferred = undefined;
    this.hasDeferred = false;
    this.ended = false;
  }

  // The window index of the next source row, refilling the window with up to `cap` rows from `pull`
  // (null at the source's end; a throw is a source error) once it is spent; null at the source's end;
  // or a throw of the deferred source error once every row read before it has been handed out.
  next(cap: number, pull: () => Row | null): number | null {
    if (this.pos === this.rows.length) {
      if (this.hasDeferred) this.raiseDeferred();
      if (this.ended) return null;
      this.rows = [];
      this.pos = 0;
      this.filter?.reset();
      this.project?.reset();
      while (this.rows.length < Math.max(cap, 1)) {
        let row: Row | null;
        try {
          row = pull();
        } catch (e) {
          this.deferred = e;
          this.hasDeferred = true;
          break;
        }
        if (row === null) {
          this.ended = true;
          break;
        }
        this.rows.push(row);
      }
      if (this.rows.length === 0) {
        if (this.hasDeferred) this.raiseDeferred();
        return null;
      }
    }
    return this.pos++;
  }

  // Make window row `i` current for `batch` (one of this window's, or null), prefetching from it if
  // needed.
  setRow(batch: HostBatch | null, i: number, env: EvalEnv, meter: Meter): void {
    batch?.setRow(i, this.rows, this.rows.length, env, meter);
  }

  private raiseDeferred(): never {
    const e = this.deferred;
    this.deferred = undefined;
    this.hasDeferred = false;
    throw e;
  }
}

// The read-ahead window size (extensibility.md §4.2.1): at most READ_AHEAD_ROWS; on a metered handle
// at most the rows the remaining budget could pay `unit` (the per-row charge replay makes first) for,
// plus the one that trips it; and at most `remaining` (when not null), the rows the site can still
// emit.
export function readAheadCap(meter: Meter, unit: bigint, remaining: bigint | null): number {
  let n = READ_AHEAD_ROWS;
  if (remaining !== null && remaining < BigInt(n)) n = Number(remaining > 1n ? remaining : 1n);
  const h = meter.headroom();
  if (h !== undefined && unit > 0n) {
    const payable = (h > 0n ? h : 0n) / unit;
    if (payable < BigInt(n)) n = Number(payable) + 1;
  }
  return n;
}
