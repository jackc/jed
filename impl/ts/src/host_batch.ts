// Host-function batch prefetch (spec/design/extensibility.md §4.2.1) — speculative batch, scalar
// replay. An evaluation site that already holds a chunk of rows asks HostBatch.setRow to call each
// eligible host function ONCE over the chunk's argument columns, then replays its ordinary
// row-at-a-time evaluation with the batch installed on the EvalEnv (env.hostBatch). The hostFunc eval
// case still charges, guards, and type-checks per row exactly as before, and evaluates its arguments
// and short-circuits NULL for any row the batch cannot answer; for a prefetched row it takes the
// kernel's outcome from the cache, skipping the argument evaluation that would charge nothing, raise
// nothing, and not short-circuit. So rows, cost, abort points, and errors are identical to
// batch-of-one by construction. Mirrors impl/rust/src/executor/host_batch.rs.

import { Meter } from "./cost.ts";
import { evalExpr } from "./eval.ts";
import type { EvalEnv, RExpr } from "./executor.ts";
import type { ExtensionRegistry, HostOutcome } from "./extension.ts";
import type { Row } from "./storage.ts";
import type { Value } from "./value.ts";

// The largest chunk a site prefetches (extensibility.md §4.2.1).
export const HOST_BATCH_ROWS = 1024;

type HostFuncNode = Extract<RExpr, { kind: "hostFunc" }>;

// One eligible call node's prefetched outcomes over the site's current chunk.
type Slot = {
  // The call node itself (an item of the site's expression list) — compared by identity. A node
  // outside this site's expression list (a subquery's) never matches.
  node: HostFuncNode;
  // The site row index of outcomes[0].
  base: number;
  // Per chunk row: the kernel's outcome, or undefined (not prefetched — a NULL / unevaluable argument,
  // a row past a reported error, or a row outside the chunk). Taken (cleared) on replay.
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

  // The batch for a site that evaluates every expression of `exprs` once per row, or null when none
  // is eligible (§4.2.1): an item that is itself a call to a non-volatile host function whose
  // arguments are all trivially evaluable.
  static forExprs(exprs: RExpr[], registry: ExtensionRegistry | null): HostBatch | null {
    if (registry === null) return null;
    const slots: Slot[] = [];
    for (const e of exprs) {
      if (e.kind !== "hostFunc") continue;
      if (!registry.functionAt(e.id).batchable() || !e.args.every(triviallyEvaluable)) continue;
      slots.push({ node: e, base: 0, outcomes: [] });
    }
    return slots.length === 0 ? null : new HostBatch(slots, registry);
  }

  // Make row `i` of the site current. When a slot's chunk does not cover it, prefetch a new chunk
  // starting at `i` over rows[i..end) (at most HOST_BATCH_ROWS rows, further capped by the meter's
  // headroom).
  setRow(i: number, rows: Row[], end: number, env: EvalEnv, meter: Meter): void {
    this.row = i;
    const headroom = meter.headroom();
    for (const slot of this.slots) {
      const covered = i >= slot.base && i < slot.base + slot.outcomes.length;
      if (!covered) this.prefetch(slot, i, rows, end, env, headroom);
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
    slot.outcomes = new Array<HostOutcome | undefined>(n).fill(undefined);
    // Gather the argument columns over the chunk rows whose arguments are all non-NULL; `members`
    // maps each batch row back to its chunk row.
    const cols: Value[][] = call.args.map(() => []);
    const members: number[] = [];
    const scratch = new Meter();
    for (let k = 0; k < n; k++) {
      const row = rows[start + k]!;
      const vals: Value[] = [];
      let ok = true;
      for (const a of call.args) {
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
    for (let m = 0; m < members.length; m++) slot.outcomes[members[m]!] = outcomes[m];
  }
}
