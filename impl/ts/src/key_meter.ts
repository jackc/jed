// Per-row key metering (spec/design/cost.md §3 "sort_key / hash_key"). A sort, a grouping or dedup
// table, and a window's partitioning charge each row's key values once, by size, where the row enters
// the operator. The comparisons and probes inside them stay unmetered. Each charge is guarded when it
// is nonzero, so a fixed-width or NULL key moves no abort point.

import type { Collation } from "./collation.ts";
import type { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import { codePointCount, payloadBytes } from "./memsize.ts";
import type { Value } from "./value.ts";

// collatedCodePoints is the code points a collated key's UCA sort key is built from: a text's, or a
// text[]'s non-NULL elements'.
function collatedCodePoints(v: Value): number {
  if (v.kind === "text") return codePointCount(v.text);
  if (v.kind === "array") {
    let n = 0;
    for (const e of v.elements) if (e.kind === "text") n += codePointCount(e.text);
    return n;
  }
  return 0;
}

// chargeSortKey charges one sort-key value of a row the sort receives: collate per code point for a
// collated text or text[] key, else sort_key per payload byte.
export function chargeSortKey(m: Meter, v: Value, coll: Collation | null): void {
  const units =
    coll !== null && (v.kind === "text" || v.kind === "array")
      ? COSTS.collate * BigInt(collatedCodePoints(v))
      : COSTS.sortKey * BigInt(payloadBytes(v));
  if (units === 0n) return;
  m.charge(units);
  m.guard();
}

// chargeSortKeys charges every sort key of one row a sort receives, in key order. A collated sort
// charges through collationKeysForRow instead, which interleaves each key's charge with its
// decoration.
export function chargeSortKeys(
  m: Meter,
  row: Value[],
  order: readonly { idx: number; collation: Collation | null }[],
): void {
  for (const k of order) chargeSortKey(m, row[k.idx]!, k.collation);
}

// chargeHashKey charges one key value a grouping, dedup, or partition table receives: hash_key per
// payload byte.
export function chargeHashKey(m: Meter, v: Value): void {
  const units = COSTS.hashKey * BigInt(payloadBytes(v));
  if (units === 0n) return;
  m.charge(units);
  m.guard();
}

// chargeHashKeys charges a row's key values, in key order.
export function chargeHashKeys(m: Meter, keys: readonly Value[]): void {
  for (const v of keys) chargeHashKey(m, v);
}

// hashKeyUnits is the hash_key units of a row's values, uncharged (the recursive CTE's dedicated
// meter).
export function hashKeyUnits(row: readonly Value[]): bigint {
  let n = 0;
  for (const v of row) n += payloadBytes(v);
  return COSTS.hashKey * BigInt(n);
}
