// or3 is three-valued OR (Kleene): used to build <= / >= from < / > and =, so a NULL
// operand yields UNKNOWN rather than a wrong FALSE (CLAUDE.md §4).
import type { ThreeValued, Value } from "./value.ts";
import {
  compareBytea,
  compareTextC,
  float64Value,
  floatTotalCmp,
  intValue,
  isTrue,
  nullValue,
} from "./value.ts";
import type { Row } from "./storage.ts";
import type {
  EvalEnv,
  OrderSlot,
  RExpr,
  ResolvedBound,
  ResolvedFrame,
  WindowSpec,
} from "./executor.ts";
import type { FrameExclusion } from "./ast.ts";
import type { Meter } from "./cost.ts";
import {
  cloneAcc,
  cmpBytes,
  coerceForStore,
  distinctRowKey,
  evalExpr,
  finalizeAcc,
  foldAcc,
  newAcc,
  storeValue,
  unfoldAcc,
} from "./executor.ts";
import { COSTS } from "./costs.ts";
import { engineError } from "./errors.ts";
import { sortKey as collationSortKey } from "./collation.ts";
import { intervalCmp } from "./interval.ts";
import { rangeTotalCmp } from "./range.ts";
import { jsonNodeCmp } from "./json.ts";
import type { DecimalTypmod, ScalarType } from "./types.ts";
import type { ColType } from "./catalog.ts";
export function or3(
  a: "true" | "false" | "unknown",
  b: "true" | "false" | "unknown",
): "true" | "false" | "unknown" {
  if (a === "true" || b === "true") return "true";
  if (a === "unknown" || b === "unknown") return "unknown";
  return "false";
}

// not3 is three-valued NOT (Kleene): true<->false, unknown stays unknown. Used to build `<>`
// as the negation of `=`, so a NULL operand still yields UNKNOWN (`NULL <> NULL`), not a wrong TRUE.
export function not3(a: ThreeValued): ThreeValued {
  if (a === "true") return "false";
  if (a === "false") return "true";
  return "unknown";
}

// offsetCount is the integer count of a ROWS/GROUPS offset bound (an int Value by construction),
// clamped to [0, np] so a huge literal offset cannot overflow — any offset >= np already saturates
// the bound to the partition edge. Mirrors Rust's i128 widening.
export function offsetCount(v: Value, np: number): number {
  if (v.kind === "int") return v.int > BigInt(np) ? np : Number(v.int);
  return 0;
}

// rangeVVsBound returns the sign of v - (cur ∓ off) for a RANGE value offset (window.md §6),
// computed exactly: integer keys use bigint so the bound cannot overflow (matching Rust's i128);
// decimal keys use exact decimal arithmetic; float keys widen to f64 and compute the bound with the
// in-contract correctly-rounded +/- kernel (float.md §5 — bit-identical cross-core), comparing with
// the PG float total order (floatTotalCmp). The total order reproduces PG's in_range NaN handling for
// free: a NaN current key makes the bound NaN (NaN ∓ finite = NaN), so a NaN row equals it and any
// non-NaN row is below it, while a NaN row against a non-NaN bound sorts above. The offset is always
// finite (an int offset, or a decimal one that would otherwise overflow already trapped at resolve),
// so cur ∓ off never produces NaN itself. subtract chooses cur - off vs cur + off. Mirrors Rust's
// range_v_vs_bound.
export function rangeVVsBound(v: Value, cur: Value, off: Value, subtract: boolean): number {
  if (cur.kind === "int" && off.kind === "int" && v.kind === "int") {
    const b = subtract ? cur.int - off.int : cur.int + off.int;
    return v.int < b ? -1 : v.int > b ? 1 : 0;
  }
  if (cur.kind === "decimal" && off.kind === "decimal" && v.kind === "decimal") {
    const b = subtract ? cur.dec.sub(off.dec) : cur.dec.add(off.dec);
    return v.dec.cmpValue(b);
  }
  // Float key: f32 values are already Math.fround'd, so `.value` is the exact f64 widening (PG
  // computes in_range_float*_float8's sum in float8 even for an f32 key).
  if (
    (cur.kind === "f32" || cur.kind === "f64") &&
    off.kind === "f64" &&
    (v.kind === "f32" || v.kind === "f64")
  ) {
    const b = subtract ? cur.value - off.value : cur.value + off.value;
    return floatTotalCmp(v.value, b);
  }
  throw new Error("range offset resolved to a matching numeric type");
}

// FrameCtx holds one partition's peer-group structure (window.md §3/§6), shared across every row's
// frame lookup. Peers are rows equal on the window ORDER BY keys; peerStart/peerEnd bracket each
// row's peer group, groupOf is its peer-group ordinal, and groupSpans lists every group's [start,
// end). Mirrors Rust's FrameCtx.
export class FrameCtx {
  readonly np: number;
  private readonly ordered: number[];
  private readonly rows: Row[];
  private readonly order: OrderSlot[];
  private readonly peerStart: number[];
  private readonly peerEnd: number[];
  private readonly groupOf: number[];
  private readonly groupSpans: Array<[number, number]>;

  constructor(
    ordered: number[],
    rows: Row[],
    order: OrderSlot[],
    collKeys: (Uint8Array | null)[][] | null,
  ) {
    this.ordered = ordered;
    this.rows = rows;
    this.order = order;
    const np = ordered.length;
    this.np = np;
    const groupSpans: Array<[number, number]> = [];
    let s = 0;
    for (let pos = 1; pos < np; pos++) {
      if (cmpWindowRows(ordered[pos]!, ordered[s]!, rows, order, collKeys) !== 0) {
        groupSpans.push([s, pos]);
        s = pos;
      }
    }
    if (np > 0) groupSpans.push([s, np]);
    const peerStart = new Array<number>(np).fill(0);
    const peerEnd = new Array<number>(np).fill(0);
    const groupOf = new Array<number>(np).fill(0);
    for (let gi = 0; gi < groupSpans.length; gi++) {
      const [a, b] = groupSpans[gi]!;
      for (let p = a; p < b; p++) {
        peerStart[p] = a;
        peerEnd[p] = b;
        groupOf[p] = gi;
      }
    }
    this.peerStart = peerStart;
    this.peerEnd = peerEnd;
    this.groupOf = groupOf;
    this.groupSpans = groupSpans;
  }

  // bounds returns the [lo, hi) frame for the row at sorted position pos (window.md §6). A
  // null/undefined frame ⇒ the default frame (RANGE UNBOUNDED PRECEDING TO CURRENT ROW = [0, peerEnd)).
  bounds(pos: number, frame: ResolvedFrame | null | undefined): [number, number] {
    if (frame === null || frame === undefined) return [0, this.peerEnd[pos]!];
    switch (frame.mode) {
      case "rows":
        return this.rowsBounds(pos, frame);
      case "groups":
        return this.groupsBounds(pos, frame);
      default: // "range"
        return this.rangeBounds(pos, frame);
    }
  }

  // isExcluded reports whether sorted position k is dropped from the current row pos's frame by
  // EXCLUDE (window.md §6): currentRow drops the row itself, group its whole peer group, ties the
  // peers but not the row, noOthers nothing. Exclusion removes only rows already in [lo, hi).
  isExcluded(pos: number, k: number, exclude: FrameExclusion): boolean {
    switch (exclude) {
      case "currentRow":
        return k === pos;
      case "group":
        return this.peerStart[pos]! <= k && k < this.peerEnd[pos]!;
      case "ties":
        return k !== pos && this.peerStart[pos]! <= k && k < this.peerEnd[pos]!;
      default: // "noOthers"
        return false;
    }
  }

  // ROWS: physical row offsets in the partition sequence; bounds clamp to [0, np].
  private rowsBounds(pos: number, f: ResolvedFrame): [number, number] {
    const np = this.np;
    const idx = (b: ResolvedBound, isEnd: boolean): number => {
      switch (b.kind) {
        case "unboundedPreceding":
          return 0;
        case "preceding":
          return pos - offsetCount(b.offset, np) + (isEnd ? 1 : 0);
        case "currentRow":
          return pos + (isEnd ? 1 : 0);
        case "following":
          return pos + offsetCount(b.offset, np) + (isEnd ? 1 : 0);
        case "unboundedFollowing":
          return np;
      }
    };
    const clamp = (x: number): number => (x < 0 ? 0 : x > np ? np : x);
    const lo = clamp(idx(f.start, false));
    const hi = clamp(idx(f.end, true));
    return [lo, Math.max(hi, lo)];
  }

  // GROUPS: peer-group offsets — a bound g PRECEDING/FOLLOWING lands on the cg ∓ g-th peer group's
  // start (a start bound) or end (an end bound); a group index below 0 clamps to the partition
  // start, at or above the group count to the partition end.
  private groupsBounds(pos: number, f: ResolvedFrame): [number, number] {
    const np = this.np;
    const cg = this.groupOf[pos]!;
    const g = this.groupSpans.length;
    const startAt = (j: number): number => (j < 0 ? 0 : j >= g ? np : this.groupSpans[j]![0]);
    const endAt = (j: number): number => (j < 0 ? 0 : j >= g ? np : this.groupSpans[j]![1]);
    const loFor = (b: ResolvedBound): number => {
      switch (b.kind) {
        case "unboundedPreceding":
          return 0;
        case "preceding":
          return startAt(cg - offsetCount(b.offset, np));
        case "currentRow":
          return startAt(cg);
        case "following":
          return startAt(cg + offsetCount(b.offset, np));
        case "unboundedFollowing":
          return np;
      }
    };
    const hiFor = (b: ResolvedBound): number => {
      switch (b.kind) {
        case "unboundedPreceding":
          return 0;
        case "preceding":
          return endAt(cg - offsetCount(b.offset, np));
        case "currentRow":
          return endAt(cg);
        case "following":
          return endAt(cg + offsetCount(b.offset, np));
        case "unboundedFollowing":
          return np;
      }
    };
    const lo = loFor(f.start);
    const hi = hiFor(f.end);
    return [lo, Math.max(hi, lo)];
  }

  // RANGE: logical offsets on the single ordering-key value (window.md §6). A bound with no offset
  // (UNBOUNDED / CURRENT ROW) is peer/edge based and needs no key arithmetic. With a value offset,
  // the frame spans the rows whose key is within the offset of the current key; a NULL current key
  // has only its NULL peers (offset/CURRENT bounds collapse to the peer group, the PG rule), while
  // UNBOUNDED bounds still reach the partition edge.
  private rangeBounds(pos: number, f: ResolvedFrame): [number, number] {
    const np = this.np;
    const startOff = f.start.kind === "preceding" || f.start.kind === "following";
    const endOff = f.end.kind === "preceding" || f.end.kind === "following";
    if (!startOff && !endOff) {
      const lo = f.start.kind === "unboundedPreceding" ? 0 : this.peerStart[pos]!;
      const hi = f.end.kind === "unboundedFollowing" ? np : this.peerEnd[pos]!;
      return [lo, Math.max(hi, lo)];
    }
    // Offset present ⇒ exactly one ORDER BY key (validated at resolve).
    const col = this.order[0]!.idx;
    const desc = this.order[0]!.descending;
    const cur = this.rows[this.ordered[pos]!]![col]!;
    if (cur.kind === "null") {
      const lo = f.start.kind === "unboundedPreceding" ? 0 : this.peerStart[pos]!;
      const hi = f.end.kind === "unboundedFollowing" ? np : this.peerEnd[pos]!;
      return [lo, Math.max(hi, lo)];
    }
    let lo: number;
    switch (f.start.kind) {
      case "unboundedPreceding":
        lo = 0;
        break;
      case "currentRow":
        lo = this.peerStart[pos]!;
        break;
      case "preceding":
        lo = this.rangeStart(col, cur, f.start.offset, true, desc);
        break;
      case "following":
        lo = this.rangeStart(col, cur, f.start.offset, false, desc);
        break;
      case "unboundedFollowing":
        lo = np;
        break;
    }
    let hi: number;
    switch (f.end.kind) {
      case "unboundedFollowing":
        hi = np;
        break;
      case "currentRow":
        hi = this.peerEnd[pos]!;
        break;
      case "preceding":
        hi = this.rangeEnd(col, cur, f.end.offset, true, desc, lo);
        break;
      case "following":
        hi = this.rangeEnd(col, cur, f.end.offset, false, desc, lo);
        break;
      case "unboundedPreceding":
        hi = 0;
        break;
    }
    return [lo, Math.max(hi, lo)];
  }

  // The first sorted position whose key satisfies a RANGE start bound (NULL keys never qualify for a
  // non-NULL current row). subtract = isPreceding XOR descending chooses the bound side.
  private rangeStart(
    col: number,
    cur: Value,
    off: Value,
    isPreceding: boolean,
    desc: boolean,
  ): number {
    const subtract = isPreceding !== desc;
    for (let i = 0; i < this.np; i++) {
      const v = this.rows[this.ordered[i]!]![col]!;
      if (v.kind === "null") continue;
      const ord = rangeVVsBound(v, cur, off, subtract);
      // ascending frame: v >= bound; descending frame: v <= bound.
      const include = desc ? ord <= 0 : ord >= 0;
      if (include) return i;
    }
    return this.np;
  }

  // The exclusive end of a RANGE end bound, scanning forward from lo while the key stays in frame
  // (the in-frame keys form a contiguous run over the sorted partition).
  private rangeEnd(
    col: number,
    cur: Value,
    off: Value,
    isPreceding: boolean,
    desc: boolean,
    lo: number,
  ): number {
    const subtract = isPreceding !== desc;
    let hi = lo;
    for (let i = lo; i < this.np; i++) {
      const v = this.rows[this.ordered[i]!]![col]!;
      if (v.kind === "null") break;
      const ord = rangeVVsBound(v, cur, off, subtract);
      // ascending frame: v <= bound; descending frame: v >= bound.
      const include = desc ? ord >= 0 : ord <= 0;
      if (include) hi = i + 1;
      else break;
    }
    return hi;
  }
}

// applyWindowStage is the WINDOW stage (spec/design/window.md §5.2): for each window function,
// partition the rows, sort each partition by the window ORDER BY (stable → PK tie-break, as `rows`
// arrives in PK scan order), compute the per-row result, and APPEND it to every row (so window
// result i lands at flat slot inputWidth + i, where the projection reads it). The partition + sort
// are unmetered (like ORDER BY / GROUP BY); each computed result charges windowResult and guards
// the ceiling. S0: row_number() only; partitions bucket value-canonically via an insertion-ordered
// list keyed by the value-canonical distinctRowKey (the aggregate-grouping discipline), so no
// hash-map iteration order leaks (CLAUDE.md §8/§10).
//
// The frame-sensitive plans (aggregate windows, first/last/nth_value) use a FrameCtx, which
// precomputes the partition's peer-group structure once and maps each row to its [lo, hi) frame.
// spec/design/window.md §6.

// groupWindowSpecs groups window specs that share an identical PARTITION BY + ORDER BY (column
// slots + direction / NULLS / collation; collations are interned so the reference compares equal),
// returning the spec indices per group. One partition + per-partition sort then serves every spec in
// a group (window.md §5.2 — the shared partition/sort pass). Grouping is stable and the per-spec slot
// mapping is preserved (each spec still writes its result column in spec order), so the optimization
// is purely a wall-clock win — the cost is unchanged (§8).
export function groupWindowSpecs(specs: WindowSpec[]): number[][] {
  const groups: number[][] = [];
  const orderEq = (a: OrderSlot[], b: OrderSlot[]): boolean =>
    a.length === b.length &&
    a.every(
      (s, i) =>
        s.idx === b[i]!.idx &&
        s.descending === b[i]!.descending &&
        s.nullsFirst === b[i]!.nullsFirst &&
        s.collation === b[i]!.collation,
    );
  const partEq = (a: number[], b: number[]): boolean =>
    a.length === b.length && a.every((p, i) => p === b[i]);
  outer: for (let i = 0; i < specs.length; i++) {
    for (const g of groups) {
      const rep = specs[g[0]!]!;
      if (partEq(rep.partition, specs[i]!.partition) && orderEq(rep.order, specs[i]!.order)) {
        g.push(i);
        continue outer;
      }
    }
    groups.push([i]);
  }
  return groups;
}

// materializeOrderExprs materializes the general-expression ORDER BY keys before the sort
// (spec/design/grammar.md §10): for each row evaluate every orderExprs[k] and append the value, so its
// sort slot finalWidth+k reads the appended column and the slot-based comparator stays unchanged — the
// exact mechanism a non-column window key uses (window.md §5.1, applyWindowStage). Runs over every
// pre-sort row (before LIMIT, since the sort needs them all); the per-row evaluation is metered like a
// projection (operator_eval per node, charged inside evalExpr). A no-op — and zero added cost — when
// orderExprs is empty (a column/ordinal-only ORDER BY, byte-identical to before).
export function materializeOrderExprs(
  rows: Row[],
  orderExprs: RExpr[],
  env: EvalEnv,
  meter: Meter,
): void {
  if (orderExprs.length === 0) return;
  for (let i = 0; i < rows.length; i++) {
    // Detach from the (possibly shared) stored row before appending, exactly as the window stage does
    // — the scan yields references to the page store's own arrays, so appending in place would corrupt
    // them across statements. A synthetic group row is already private; the extra copy is harmless.
    const row = rows[i]!.slice();
    const vals: Value[] = [];
    for (const oe of orderExprs) vals.push(evalExpr(oe, row, env, meter));
    for (const v of vals) row.push(v);
    rows[i] = row;
  }
}

export function applyWindowStage(
  rows: Row[],
  specs: WindowSpec[],
  windowKeys: RExpr[],
  env: EvalEnv,
  meter: Meter,
): void {
  const n = rows.length;
  if (n === 0) return;
  // Copy each input row to a fresh array BEFORE appending: the scan yields references to the stored
  // table rows (the page store's own arrays), so appending in place would corrupt them across
  // statements. The window stage owns its row buffer (Rust holds owned Rows; the TS scan shares
  // them), so detach here, then push the per-row results onto these private copies.
  for (let i = 0; i < n; i++) rows[i] = rows[i]!.slice();
  // Materialize the non-column PARTITION BY / ORDER BY key expressions (window.md §5.1): evaluate
  // each against the row and append it, so a materialized key's slot inputWidth+k reads the appended
  // column and the partition / sort / frame machinery below (all slot-based) is unchanged. The window
  // results are appended AFTER these, so a result slot is inputWidth+windowKeys.length+w (the rebased
  // projection slot). Empty for a column-only window — no appended columns, the result slot stays
  // inputWidth+w, byte-identical to before. The key evaluation is metered like any expression
  // (operator_eval per node): new, deterministic, cross-core-identical work that exists only for an
  // expression key (a bare-column key is not in windowKeys).
  if (windowKeys.length > 0) {
    for (let i = 0; i < n; i++) {
      const row = rows[i]!;
      const kv: Value[] = [];
      for (const ke of windowKeys) kv.push(evalExpr(ke, row, env, meter));
      for (const v of kv) row.push(v);
    }
  }
  // The shared partition/sort pass (window.md §5.2): specs that share an identical PARTITION BY +
  // ORDER BY are partitioned and sorted ONCE (the expensive step), then each computes its own
  // results over the shared sorted partitions. The partition + sort are unmetered (§8), so this is
  // purely a wall-clock win — the per-spec result/frame metering, and thus the cost, are unchanged.
  const groups = groupWindowSpecs(specs);
  const specGroup = new Array<number>(specs.length).fill(0);
  const cache: Array<{ partitions: number[][]; collKeys: (Uint8Array | null)[][] | null }> = [];
  for (let gi = 0; gi < groups.length; gi++) {
    const group = groups[gi]!;
    const rep = specs[group[0]!]!;
    for (const si of group) specGroup[si] = gi;
    // Partition the row indices by the partition-key values. The Map is an index only (never
    // iterated); output comes from the insertion-ordered `partitions` (no hash-order leak).
    const index = new Map<string, number>();
    const partitions: number[][] = [];
    for (let i = 0; i < n; i++) {
      const keyVals = rep.partition.map((p) => rows[i]![p]!);
      const k = distinctRowKey(keyVals);
      let pi = index.get(k);
      if (pi === undefined) {
        pi = partitions.length;
        index.set(k, pi);
        partitions.push([]);
      }
      partitions[pi]!.push(i);
    }
    // Collated UCA sort-key bytes for the shared order's collated slots (window.md §3/§5); null
    // when no key is collated, an unmapped code point throws 0A000 here.
    const collKeys = windowCollKeys(rows, rep.order);
    // Sort each partition by the shared window ORDER BY. Array#sort is stable, so a full tie keeps
    // ascending original index = PK scan order (the §3 PK tie-break).
    if (rep.order.length > 0) {
      for (const part of partitions) {
        part.sort((a, b) => cmpWindowRows(a, b, rows, rep.order, collKeys));
      }
    }
    cache.push({ partitions, collKeys });
  }
  for (let si = 0; si < specs.length; si++) {
    const spec = specs[si]!;
    const shared = cache[specGroup[si]!]!;
    const collKeys = shared.collKeys;
    // Compute each row's result into a per-row slot, then append in input order.
    const results: Value[] = new Array(n).fill(nullValue());
    for (const ordered of shared.partitions) {
      switch (spec.plan) {
        case "rowNumber":
          for (let pos = 0; pos < ordered.length; pos++) {
            meter.guard(); // enforce the cost ceiling per result (CLAUDE.md §13)
            meter.charge(COSTS.windowResult);
            results[ordered[pos]!] = intValue(BigInt(pos + 1));
          }
          break;
        case "rank":
        case "denseRank":
        case "percentRank":
        case "cumeDist": {
          // Peer-aware ranking (window.md §3/§4): peers are rows EQUAL on the window ORDER BY keys
          // only. A single pass identifies peer-group spans [start, end) over the sorted partition;
          // an empty ORDER BY makes the whole partition one peer group. rank = start+1, dense_rank =
          // group ordinal, percent_rank = start/(N-1) (0 if N=1), cume_dist = end/N. The ratios are
          // f64 (PG's float8, window.md §4): one IEEE correctly-rounded division of small integers
          // that convert exactly to binary64, so the value is bit-identical across cores and to PG
          // (the in-contract kernel, float.md §5).
          const np = ordered.length;
          const groups: Array<[number, number]> = []; // peer-group spans [start, end)
          let s = 0;
          for (let pos = 1; pos < np; pos++) {
            if (cmpWindowRows(ordered[pos]!, ordered[s]!, rows, spec.order, collKeys) !== 0) {
              groups.push([s, pos]);
              s = pos;
            }
          }
          if (np > 0) groups.push([s, np]);
          for (let gi = 0; gi < groups.length; gi++) {
            const [start, end] = groups[gi]!;
            for (let k = start; k < end; k++) {
              const ri = ordered[k]!;
              meter.guard();
              meter.charge(COSTS.windowResult);
              if (spec.plan === "rank") {
                results[ri] = intValue(BigInt(start + 1));
              } else if (spec.plan === "denseRank") {
                results[ri] = intValue(BigInt(gi + 1));
              } else if (spec.plan === "percentRank") {
                results[ri] = np <= 1 ? float64Value(0.0) : float64Value(start / (np - 1));
              } else {
                results[ri] = float64Value(end / np);
              }
            }
          }
          break;
        }
        // ntile(n): distribute the partition into n ranked buckets, larger buckets first
        // (window.md §4). n is evaluated once (the first sorted row); NULL n → NULL for all;
        // n ≤ 0 → 22014. Position-based: bucket boundaries are by sorted position, not peers.
        case "ntile": {
          const np = ordered.length;
          const nval = evalExpr(spec.args[0]!, rows[ordered[0]!]!, env, meter);
          if (nval.kind === "null") {
            // NULL bucket count → NULL for every row (PG).
            for (const ri of ordered) {
              meter.guard();
              meter.charge(COSTS.windowResult);
              results[ri] = nullValue();
            }
          } else if (nval.kind === "int") {
            if (nval.int <= 0n) {
              throw engineError(
                "invalid_argument_for_ntile",
                "argument of ntile must be greater than zero",
              );
            }
            // np is a safe number; nbuckets is an i64 bigint. base = floor(np/nbuckets),
            // rem = np % nbuckets, big = rem*(base+1) — computed in bigint to avoid any
            // precision loss for a huge nbuckets, then narrowed to number (all ≤ np, safe).
            const npb = BigInt(np);
            const base = Number(npb / nval.int); // floor rows per bucket
            const rem = Number(npb % nval.int); // the first `rem` buckets get one extra row
            const big = rem * (base + 1); // rows in the larger (base+1) buckets
            for (let pos = 0; pos < ordered.length; pos++) {
              const ri = ordered[pos]!;
              meter.guard();
              meter.charge(COSTS.windowResult);
              // Larger buckets first: positions [0, big) → (base+1)-sized buckets, the rest →
              // base-sized buckets. `base` is 0 only when nbuckets > np, and then every pos < big
              // so the else branch never divides by 0.
              const bucket =
                pos < big
                  ? Math.floor(pos / (base + 1)) + 1
                  : rem + Math.floor((pos - big) / base) + 1;
              results[ri] = intValue(BigInt(bucket));
            }
          }
          break;
        }
        // lag/lead (window.md §4): the value `offset` positions back (lag) / forward (lead) in the
        // partition, else the default (or NULL). Frame-insensitive — offset is by sorted position.
        // The value is evaluated for every row; offset once (NULL → all NULL); the default per
        // out-of-range row.
        case "lag":
        case "lead": {
          const np = ordered.length;
          const vals: Value[] = new Array(np);
          for (let i = 0; i < np; i++) {
            vals[i] = evalExpr(spec.args[0]!, rows[ordered[i]!]!, env, meter);
          }
          // offset: evaluated once from the first sorted row. NULL → NULL for every row (PG);
          // absent → 1. A small offset, but compared in number space (a huge offset just lands
          // out of range → default/NULL).
          let offset: number | null;
          if (spec.args.length >= 2) {
            const ov = evalExpr(spec.args[1]!, rows[ordered[0]!]!, env, meter);
            offset = ov.kind === "null" ? null : Number((ov as { int: bigint }).int);
          } else {
            offset = 1;
          }
          const dir = spec.plan === "lead" ? 1 : -1;
          for (let pos = 0; pos < np; pos++) {
            const ri = ordered[pos]!;
            meter.guard();
            meter.charge(COSTS.windowResult);
            if (offset === null) {
              results[ri] = nullValue();
            } else {
              const target = pos + dir * offset;
              if (target >= 0 && target < np) {
                results[ri] = vals[target]!;
              } else if (spec.args.length === 3) {
                results[ri] = evalExpr(spec.args[2]!, rows[ri]!, env, meter);
              } else {
                results[ri] = nullValue();
              }
            }
          }
          break;
        }
        // An aggregate over the default frame (window.md §6): RANGE UNBOUNDED PRECEDING TO CURRENT
        // ROW with a window ORDER BY (a RUNNING aggregate — CURRENT ROW spans the current peer
        // group), or the WHOLE partition with no ORDER BY. Both reduce to the same shape: fold rows
        // in sorted order, snapshotting the running Acc at each peer-group boundary (no ORDER BY →
        // one peer group → one whole-partition value).
        case "agg": {
          const np = ordered.length;
          const hasOperand = spec.args.length > 0; // COUNT(*) has no operand
          // FILTER (WHERE cond): a frame row whose filter is not TRUE does not fold into the window
          // aggregate (aggregates.md §20). Evaluated per visited frame row (charging its
          // operatorEvals); a null filter keeps every row. A FILTER forces the naive re-fold path for
          // explicit frames (a filtered row cannot be cleanly un-folded).
          const filterPass = (k: number): boolean =>
            spec.filter == null
              ? true
              : isTrue(evalExpr(spec.filter, rows[ordered[k]!]!, env, meter));
          if (spec.frame === null || spec.frame === undefined) {
            // DEFAULT frame: a single running pass, snapshotting the accumulator at each peer-group
            // boundary (window.md §6) — O(n).
            const groups: Array<[number, number]> = []; // peer-group spans [start, end)
            let s = 0;
            for (let pos = 1; pos < np; pos++) {
              if (cmpWindowRows(ordered[pos]!, ordered[s]!, rows, spec.order, collKeys) !== 0) {
                groups.push([s, pos]);
                s = pos;
              }
            }
            if (np > 0) groups.push([s, np]);
            const acc = newAcc(
              spec.aggPlan!,
              spec.aggFloatWidth ?? "f64",
              spec.aggJsonAsJson ?? false,
              spec.aggJsonStrict ?? false,
            );
            for (const [start, end] of groups) {
              for (let k = start; k < end; k++) {
                // The frame fold work (window.md §8) — metered so a running aggregate over a large
                // partition stays cost-bounded.
                meter.charge(COSTS.windowFrameStep);
                if (!filterPass(k)) continue; // FILTER excludes this row from the running fold
                const v = hasOperand
                  ? evalExpr(spec.args[0]!, rows[ordered[k]!]!, env, meter)
                  : nullValue();
                foldAcc(acc, v, meter);
              }
              // Snapshot the running accumulator for this peer group's frame [0, end).
              const out = finalizeAcc(cloneAcc(acc));
              for (let k = start; k < end; k++) {
                const ri = ordered[k]!;
                meter.guard();
                meter.charge(COSTS.windowResult);
                results[ri] = out;
              }
            }
          } else {
            // EXPLICIT frame (window.md §5.2/§6). The sorted partition makes the frame bounds
            // [lo, hi) monotonic non-decreasing in pos, so a NO-EXCLUDE aggregate CARRIES one
            // accumulator across rows rather than re-folding each frame from scratch (the
            // sliding-window optimization):
            //   • an EXPANDING frame (start UNBOUNDED PRECEDING ⇒ lo ≡ 0) folds each entering row
            //     once as hi advances — byte-identical for EVERY aggregate (fold order is the
            //     sorted-prefix order the naive path uses) — O(n);
            //   • a MOVING frame additionally UN-folds the rows leaving on the left, but only for
            //     the exactly-invertible COUNT / COUNT(*) — O(n);
            //   • a MOVING frame over SUM/AVG/MIN/MAX/float (not safely invertible) and ANY frame
            //     with EXCLUDE re-fold from scratch (the naive O(partition²)).
            // windowFrameStep is charged per folded AND per un-folded row, so it only LOWERS; each
            // row's operand is evaluated at most once (cached in vals), so operator_eval never rises.
            const ctx = new FrameCtx(ordered, rows, spec.order, collKeys);
            const exclude = spec.frame?.exclude ?? "noOthers";
            const vals: Value[] = new Array(np);
            const valSet: boolean[] = new Array(np).fill(false);
            const evalAt = (k: number): Value => {
              if (!hasOperand) return nullValue();
              if (!valSet[k]) {
                vals[k] = evalExpr(spec.args[0]!, rows[ordered[k]!]!, env, meter);
                valSet[k] = true;
              }
              return vals[k]!;
            };
            if (exclude !== "noOthers" || spec.filter != null) {
              // EXCLUDE or FILTER breaks the clean add/remove model → naive per-row re-fold (dropped
              // rows are neither metered nor counted), over the cached operand. A FILTER additionally
              // skips a non-TRUE frame row.
              for (let pos = 0; pos < np; pos++) {
                const [lo, hi] = ctx.bounds(pos, spec.frame);
                const acc = newAcc(
                  spec.aggPlan!,
                  spec.aggFloatWidth ?? "f64",
                  spec.aggJsonAsJson ?? false,
                  spec.aggJsonStrict ?? false,
                );
                for (let k = lo; k < hi; k++) {
                  if (ctx.isExcluded(pos, k, exclude)) continue;
                  meter.charge(COSTS.windowFrameStep);
                  if (!filterPass(k)) continue;
                  foldAcc(acc, evalAt(k), meter);
                }
                meter.guard();
                meter.charge(COSTS.windowResult);
                results[ordered[pos]!] = finalizeAcc(acc);
              }
            } else {
              // SLIDING (monotone carry). removable aggregates un-fold the left edge; the rest
              // rebuild when lo advances (an expanding frame never advances lo, so it only adds).
              const removable = spec.aggPlan === "countStar" || spec.aggPlan === "count";
              let acc = newAcc(
                spec.aggPlan!,
                spec.aggFloatWidth ?? "f64",
                spec.aggJsonAsJson ?? false,
                spec.aggJsonStrict ?? false,
              );
              let curLo = 0;
              let curHi = 0;
              for (let pos = 0; pos < np; pos++) {
                const [lo, hi] = ctx.bounds(pos, spec.frame);
                if (!removable && lo > curLo) {
                  // Left edge advanced over a non-invertible aggregate ⇒ rebuild over [lo, hi).
                  acc = newAcc(
                    spec.aggPlan!,
                    spec.aggFloatWidth ?? "f64",
                    spec.aggJsonAsJson ?? false,
                    spec.aggJsonStrict ?? false,
                  );
                  for (let k = lo; k < hi; k++) {
                    meter.charge(COSTS.windowFrameStep);
                    foldAcc(acc, evalAt(k), meter);
                  }
                } else {
                  // Un-fold rows leaving on the left (invertible only; empty when lo === curLo) …
                  const remHi = Math.min(lo, curHi);
                  for (let k = curLo; k < remHi; k++) {
                    meter.charge(COSTS.windowFrameStep);
                    unfoldAcc(acc, evalAt(k), meter);
                  }
                  // … and fold rows entering on the right.
                  const addLo = Math.max(curHi, lo);
                  for (let k = addLo; k < hi; k++) {
                    meter.charge(COSTS.windowFrameStep);
                    foldAcc(acc, evalAt(k), meter);
                  }
                }
                curLo = lo;
                curHi = hi;
                meter.guard();
                meter.charge(COSTS.windowResult);
                results[ordered[pos]!] = finalizeAcc(cloneAcc(acc));
              }
            }
          }
          break;
        }
        // Frame-sensitive value pickers (S4, window.md §4): first/last/nth row of the frame.
        case "firstValue":
        case "lastValue":
        case "nthValue": {
          const np = ordered.length;
          // The value expression, evaluated once per row (sorted order).
          const vals: Value[] = new Array(np);
          for (let i = 0; i < np; i++) {
            vals[i] = evalExpr(spec.args[0]!, rows[ordered[i]!]!, env, meter);
          }
          // nth_value's position — evaluated once; NULL → NULL for all; < 1 → 22016.
          let nth: number | null = 0; // unused for first/last
          if (spec.plan === "nthValue") {
            const nv = evalExpr(spec.args[1]!, rows[ordered[0]!]!, env, meter);
            if (nv.kind === "null") {
              nth = null;
            } else if (nv.kind === "int") {
              if (nv.int >= 1n) {
                nth = Number(nv.int);
              } else {
                throw engineError(
                  "invalid_argument_for_nth_value",
                  "argument of nth_value must be greater than zero",
                );
              }
            }
          }
          const ctx = new FrameCtx(ordered, rows, spec.order, collKeys);
          const exclude = spec.frame?.exclude ?? "noOthers";
          for (let pos = 0; pos < np; pos++) {
            meter.guard();
            meter.charge(COSTS.windowResult);
            const [lo, hi] = ctx.bounds(pos, spec.frame);
            // first/last/nth pick over the frame's NON-excluded rows (window.md §6); the noOthers
            // fast path breaks on the first row, so it stays O(1).
            let out: Value = nullValue();
            if (spec.plan === "firstValue") {
              for (let k = lo; k < hi; k++) {
                if (!ctx.isExcluded(pos, k, exclude)) {
                  out = vals[k]!;
                  break;
                }
              }
            } else if (spec.plan === "lastValue") {
              for (let k = hi - 1; k >= lo; k--) {
                if (!ctx.isExcluded(pos, k, exclude)) {
                  out = vals[k]!;
                  break;
                }
              }
            } else if (nth !== null) {
              // nth_value: the nth survivor; NULL if fewer than n survive (or NULL n).
              let count = 0;
              for (let k = lo; k < hi; k++) {
                if (ctx.isExcluded(pos, k, exclude)) continue;
                count++;
                if (count === nth) {
                  out = vals[k]!;
                  break;
                }
              }
            }
            results[ordered[pos]!] = out;
          }
          break;
        }
      }
    }
    for (let i = 0; i < n; i++) rows[i]!.push(results[i]!);
  }
}

// sortRows sorts rows by the ORDER BY keys (spec/design/grammar.md §10). The all-C fast path is a
// stable sort over the value comparator; if ANY key carries a collation, the collation-aware
// sortRowsCollated decorate sorter runs instead (it can throw — an unmapped code point is 0A000).
// (Array.prototype.sort is stable in modern engines — the runtime jed targets, spill.md §6.)
export function sortRows(rows: Row[], order: OrderSlot[]): void {
  if (order.some((k) => k.collation !== null)) {
    sortRowsCollated(rows, order);
    return;
  }
  rows.sort((a, b) => cmpRowsByOrder(a, b, order));
}

// cmpRowsByOrder compares two rows by the (all-C) ORDER BY keys — the first non-equal key decides; a
// full tie is 0 (the stable sort then keeps input order). Only used when no key is collated.
export function cmpRowsByOrder(a: Row, b: Row, order: OrderSlot[]): number {
  for (const k of order) {
    const c = keyCmp(a[k.idx]!, b[k.idx]!, k.descending, k.nullsFirst);
    if (c !== 0) return c;
  }
  return 0;
}

// windowCollKeys precomputes each row's collated UCA sort-key bytes for the spec's collated ORDER BY
// slots (if any), indexed in parallel with rows, so the partition sort AND peer determination
// (ranking, frame peer groups) honor the collation identically (window.md §3/§5). Returns null when no
// key is collated. An unmapped code point throws 0A000 here, at this deterministic per-row point.
export function windowCollKeys(rows: Row[], order: OrderSlot[]): (Uint8Array | null)[][] | null {
  if (!order.some((k) => k.collation !== null)) return null;
  return rows.map((row) => {
    const keys: (Uint8Array | null)[] = [];
    for (const k of order) {
      if (k.collation === null) continue;
      const v = row[k.idx]!;
      keys.push(v.kind === "text" ? collationSortKey(k.collation, v.text) : null);
    }
    return keys;
  });
}

// cmpWindowRows compares two rows of the window buffer (by their index a/b into the full row array) by
// the window ORDER BY keys, honoring collation. A collated slot compares the precomputed UCA sort-key
// bytes in collKeys (indexed in parallel with the rows; a null entry ⇒ a NULL value, NULL placement +
// the descending flip applied here, mirroring cmpDecorated); a non-collated slot compares the row
// values via keyCmp. This one comparator drives the partition sort AND every peer determination
// (ranking, the aggregate default frame, FrameCtx's peer groups), so a collated window orders, ranks,
// and frames identically (window.md §3/§5). With no collated key, collKeys is null and this is
// cmpRowsByOrder by index.
export function cmpWindowRows(
  a: number,
  b: number,
  rows: Row[],
  order: OrderSlot[],
  collKeys: (Uint8Array | null)[][] | null,
): number {
  let ci = 0; // advances once per collated slot (keys stored in slot order)
  for (const k of order) {
    let c: number;
    if (k.collation !== null) {
      const ak = collKeys![a]![ci] ?? null;
      const bk = collKeys![b]![ci] ?? null;
      ci++;
      if (ak === null && bk === null) c = 0;
      else if (ak === null) c = k.nullsFirst ? -1 : 1;
      else if (bk === null) c = k.nullsFirst ? 1 : -1;
      else {
        c = cmpBytes(ak, bk);
        if (k.descending) c = -c;
      }
    } else {
      c = keyCmp(rows[a]![k.idx]!, rows[b]![k.idx]!, k.descending, k.nullsFirst);
    }
    if (c !== 0) return c;
  }
  return 0;
}

// sortRowsCollated sorts rows when at least one ORDER BY key is collated (spec/design/collation.md
// §6/§8). Decorate-sort-undecorate: each collated key's UCA sort key is built ONCE per row up front
// (propagating a sortKey failure — e.g. 0A000 for an unmapped code point — at this deterministic
// per-row point, not inside the comparator), then the rows are sorted by the precomputed key bytes
// for collated slots and the value comparator for the rest. The sort is UNMETERED like every sort
// (cost.md §3); the collate cost is charged at the comparison evaluator (collation.md §11). A
// collated ORDER BY is in-memory only this slice, so this never spills (collated keys are slice 1e).
export function sortRowsCollated(rows: Row[], order: OrderSlot[]): void {
  // (keys[i], row) per row; a keys entry is null for a NULL value, the sort-key bytes otherwise.
  const deco: { keys: (Uint8Array | null)[]; row: Row }[] = rows.map((row) => {
    const keys: (Uint8Array | null)[] = [];
    for (const k of order) {
      if (k.collation === null) continue;
      const v = row[k.idx]!;
      keys.push(v.kind === "text" ? collationSortKey(k.collation, v.text) : null);
    }
    return { keys, row };
  });
  deco.sort((a, b) => cmpDecorated(a.keys, a.row, b.keys, b.row, order));
  for (let i = 0; i < deco.length; i++) rows[i] = deco[i]!.row;
}

// cmpDecorated compares two decorated rows (precomputed collated-key bytes + the row) by the ORDER BY
// keys. A collated slot compares its precomputed sort-key bytes (NULL placement + the descending flip
// applied here, mirroring keyCmp); a non-collated slot compares the row values via keyCmp.
export function cmpDecorated(
  akeys: (Uint8Array | null)[],
  arow: Row,
  bkeys: (Uint8Array | null)[],
  brow: Row,
  order: OrderSlot[],
): number {
  let ci = 0; // advances once per collated slot (keys stored in slot order)
  for (const k of order) {
    let c: number;
    if (k.collation !== null) {
      const ak = akeys[ci] ?? null;
      const bk = bkeys[ci] ?? null;
      ci++;
      if (ak === null && bk === null) c = 0;
      else if (ak === null) c = k.nullsFirst ? -1 : 1;
      else if (bk === null) c = k.nullsFirst ? 1 : -1;
      else {
        c = cmpBytes(ak, bk);
        if (k.descending) c = -c;
      }
    } else {
      c = keyCmp(arow[k.idx]!, brow[k.idx]!, k.descending, k.nullsFirst);
    }
    if (c !== 0) return c;
  }
  return 0;
}

// keyCmp is one ORDER BY key's total-order comparison, returning <0, 0, >0. NULL placement
// is governed by nullsFirst and applied INDEPENDENTLY of the value-direction flip
// (descending), so an explicit NULLS FIRST|LAST overrides the direction default
// (spec/design/grammar.md §10). The physical key order ratifies NULL as the largest value
// (the PostgreSQL model), which surfaces as the parse-time default nullsFirst = descending.
export function keyCmp(a: Value, b: Value, descending: boolean, nullsFirst: boolean): number {
  if (a.kind === "null" && b.kind === "null") return 0;
  if (a.kind === "null") return nullsFirst ? -1 : 1;
  if (b.kind === "null") return nullsFirst ? 1 : -1;
  const base = valueCmp(a, b);
  return descending ? -base : base;
}

// valueCmp is the total order over NON-NULL values: signed-integer ascending, text by
// the C collation — UTF-8 byte / code-point order (compareTextC, NOT JS `<` — the §8 trap;
// spec/design/types.md §11) — and boolean by value, false < true (orderKey maps false→0,
// true→1; types.md §9). The cross-family arms are defined only for totality — ORDER BY is
// over a single typed column, so a mixed pair is unreachable from SELECT. NULLs are handled
// by keyCmp before this is reached. Returns <0, 0, >0.
export function valueCmp(a: Value, b: Value): number {
  if (a.kind === "int" && b.kind === "int") return a.int < b.int ? -1 : a.int > b.int ? 1 : 0;
  if (a.kind === "decimal" && b.kind === "decimal") return a.dec.cmpValue(b.dec);
  // Floats by the TOTAL order (-0 == +0, NaN == NaN, NaN largest — float.md §3). ORDER BY / MIN /
  // MAX / DISTINCT over a float column reach here with same-width values (one typed column).
  if (a.kind === "f32" && b.kind === "f32") return floatTotalCmp(a.value, b.value);
  if (a.kind === "f64" && b.kind === "f64") return floatTotalCmp(a.value, b.value);
  if (a.kind === "text" && b.kind === "text") return compareTextC(a.text, b.text);
  if (a.kind === "bytea" && b.kind === "bytea") return compareBytea(a.bytes, b.bytes);
  if (a.kind === "uuid" && b.kind === "uuid") return compareBytea(a.bytes, b.bytes);
  if (a.kind === "bool" && b.kind === "bool") {
    return a.value === b.value ? 0 : a.value ? 1 : -1;
  }
  // Timestamps order by the i64 instant (-infinity < finite < infinity).
  if (a.kind === "timestamp" && b.kind === "timestamp") {
    return a.micros < b.micros ? -1 : a.micros > b.micros ? 1 : 0;
  }
  if (a.kind === "timestamptz" && b.kind === "timestamptz") {
    return a.micros < b.micros ? -1 : a.micros > b.micros ? 1 : 0;
  }
  if (a.kind === "date" && b.kind === "date") {
    return a.days < b.days ? -1 : a.days > b.days ? 1 : 0;
  }
  // Intervals order by the canonical 128-bit span (spec/design/interval.md §2).
  if (a.kind === "interval" && b.kind === "interval") return intervalCmp(a.iv, b.iv);
  // A composite sorts lexicographically, NULLs-last per field (the composite sort key —
  // spec/design/composite.md §5): the first non-equal field decides, recursing through keyCmp so
  // per-field NULL placement and nested composites are handled uniformly. The caller's descending
  // flip in keyCmp reverses the whole tuple. A row-size tie-break keeps it total (same-type rows
  // have equal arity, so it is only reached for safety).
  if (a.kind === "composite" && b.kind === "composite") {
    const n = Math.min(a.fields.length, b.fields.length);
    for (let i = 0; i < n; i++) {
      const c = keyCmp(a.fields[i]!, b.fields[i]!, false, false);
      if (c !== 0) return c;
    }
    return a.fields.length < b.fields.length ? -1 : a.fields.length > b.fields.length ? 1 : 0;
  }
  // An array sorts by the PG array_cmp total order (spec/design/array.md §5): element-wise over the
  // flattened elements (NULLs-last per element, recursing through keyCmp), then fewer elements first,
  // then smaller ndim, then per dimension (length, then lower bound).
  if (a.kind === "array" && b.kind === "array") {
    const n = Math.min(a.elements.length, b.elements.length);
    for (let i = 0; i < n; i++) {
      const c = keyCmp(a.elements[i]!, b.elements[i]!, false, false);
      if (c !== 0) return c;
    }
    if (a.elements.length !== b.elements.length)
      return a.elements.length < b.elements.length ? -1 : 1;
    if (a.dims.length !== b.dims.length) return a.dims.length < b.dims.length ? -1 : 1;
    for (let d = 0; d < a.dims.length; d++) {
      if (a.dims[d] !== b.dims[d]) return a.dims[d]! < b.dims[d]! ? -1 : 1;
      if (a.lbounds[d] !== b.lbounds[d]) return a.lbounds[d]! < b.lbounds[d]! ? -1 : 1;
    }
    return 0;
  }
  // A range sorts by the PG range_cmp total order (spec/design/ranges.md §6): `empty` below every
  // non-empty, then lower bound, then upper bound (accounting for infinity/inclusivity). Kept
  // identical to value's lt3/gt3 range arm so `<` and ORDER BY never disagree.
  if (a.kind === "range" && b.kind === "range") return rangeTotalCmp(a, b);
  // jsonb sorts by PG's total btree order (spec/design/json.md §5); kept identical to value's
  // lt3/gt3 jsonb arm so `<` and ORDER BY never disagree. (json never sorts — the resolver rejects
  // it 42883.)
  if (a.kind === "jsonb" && b.kind === "jsonb") return jsonNodeCmp(a.node, b.node);
  // Cross-family arms exist only for totality — ORDER BY is over a single typed column, so a
  // mixed pair is unreachable. A fixed family order keeps the comparator total.
  const fr = familyRank(a) - familyRank(b);
  return fr < 0 ? -1 : fr > 0 ? 1 : 0;
}

// familyRank is a fixed total order across value families, for the unreachable cross-family
// case of valueCmp (ORDER BY is single-column-typed).
export function familyRank(v: Value): number {
  switch (v.kind) {
    case "null":
      return 0;
    case "bool":
      return 1;
    case "int":
      return 2;
    case "decimal":
      return 3;
    case "f32":
      return 4;
    case "f64":
      return 5;
    case "text":
      return 6;
    case "bytea":
      return 7;
    case "uuid":
      return 8;
    case "timestamp":
      return 9;
    case "timestamptz":
      return 10;
    case "interval":
      return 11;
    case "date":
      return 13;
    // A composite sorts only against composites of its own type (ORDER BY is single-typed), so this
    // cross-family rank is only for totality; it sits after the scalar families.
    case "composite":
      return 12;
    // json never sorts (42883 at resolve); jsonb sorts only against jsonb. Cross-family ranks for
    // totality only — they sit after the scalar/container families.
    case "json":
      return 15;
    case "jsonb":
      return 16;
    case "jsonpath":
      return 17;
    default:
      return 13;
  }
}

// AssignPlan is a resolved UPDATE assignment: target column index, its type and
// nullability for re-checking, and the resolved RHS expression (evaluated against the
// old row).
export type AssignPlan = {
  idx: number;
  name: string;
  target: ScalarType;
  decimal: DecimalTypmod | null;
  // The varchar(n) length for a text column (spec/design/types.md §15) — UPDATE re-checks the new
  // value's length exactly like INSERT (over-length 22001, trailing-space truncate).
  varcharLen: number | null;
  notNull: boolean;
  source: RExpr;
  // The resolved ColType for a NON-scalar column — when set, checkAssign stores through
  // coerceForStore; absent for a scalar column, which stays on the storeValue fast path. Composite
  // columns reach this only through SET col = DEFAULT; ordinary composite assignment is deferred.
  colType?: ColType;
};

// checkAssign type-checks + coerces a candidate value against a column — the same store path
// INSERT uses (NULL into NOT NULL → 23502; an integer out of range → 22003; a decimal rounds to
// scale; a boolean into a boolean column is accepted as-is; a range/array re-coerces its
// elements). The resolver proved the value's family is assignable.
export function checkAssign(p: AssignPlan, v: Value): Value {
  if (p.colType !== undefined)
    return coerceForStore(v, p.colType, p.decimal, p.varcharLen, p.notNull, p.name);
  return storeValue(v, p.target, p.decimal, p.varcharLen, p.notNull, p.name);
}
