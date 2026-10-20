// P9 deterministic column statistics collection and snapshot state.

import type { Table } from "./catalog.ts";
import type { Collation } from "./collation.ts";
import type { Meter } from "./cost.ts";
import type { Steps } from "./execution.ts";
import { COSTS } from "./costs.ts";
import {
  STATISTICS_HISTOGRAM_BOUNDS,
  STATISTICS_KMV_HASHES,
  STATISTICS_MAX_VALUE_BYTES,
  STATISTICS_MCV_ENTRIES,
  STATISTICS_SAMPLE_ROWS,
  STATISTICS_TERM_BUCKET_WIDTH,
  STATISTICS_TERM_ENTRIES,
} from "./estimator_constants.ts";
import { encodeValue } from "./format.ts";
import { compareBytes, unboundedBound } from "./pmap.ts";
import type { TableStore } from "./storage.ts";
import { type Type, typeIsJsonb } from "./types.ts";
import type { Value } from "./value.ts";
import { encodeTypedKey, ginValueTerms, isGinElementType } from "./executor.ts";

export type StatisticsValue = { value: Value; key: Uint8Array };
export type StatisticsMcv = { value: StatisticsValue; frequency: number };
export type ColumnStatistics = {
  analyzedRows: bigint;
  stale: boolean;
  nullCount: bigint;
  widthSum: bigint;
  distinctCount: bigint | null;
  sampleRows: number;
  sampleNonNullRows: number;
  mcv: StatisticsMcv[];
  histogram: StatisticsValue[];
  // GIN term facts, present exactly for a term-eligible column (statistics.md §4.5).
  terms: TermStatistics | null;
};

// A term-eligible column's GIN term facts (statistics.md §4.5): the saturated count of term
// occurrences, the row-count estimate for an unlisted term (0 = the list is complete), and the most
// common terms with their row counts, in descending count then ascending byte order.
export type TermStatistics = {
  occurrences: bigint;
  untrackedRows: bigint;
  entries: [Uint8Array, bigint][];
};

// termEligible reports whether a column type has a GIN operator class (statistics.md §4.5): jsonb,
// or an array whose element type a GIN index admits.
export function termEligible(type: Type): boolean {
  return typeIsJsonb(type) || (type.kind === "array" && isGinElementType(type.elem));
}

type SampleRow = {
  priority: bigint;
  ordinal: bigint;
  nonnull: boolean;
  oversized: boolean;
  retained: StatisticsValue | null;
};

const I64_MAX = 9223372036854775807n;

function fnv1a64(bytes: Uint8Array): bigint {
  let hash = 0xcbf29ce484222325n;
  for (const byte of bytes) {
    hash ^= BigInt(byte);
    hash = BigInt.asUintN(64, hash * 0x100000001b3n);
  }
  return hash;
}

function distributionEligible(type: Type): boolean {
  if (type.kind === "composite" || type.kind === "record" || type.kind === "array") return false;
  return (
    type.kind === "range" ||
    (type.scalar !== "json" && type.scalar !== "jsonb" && type.scalar !== "jsonpath")
  );
}

function sampleLess(a: SampleRow, b: SampleRow): boolean {
  return a.priority < b.priority || (a.priority === b.priority && a.ordinal < b.ordinal);
}

function sampleSiftUp(heap: SampleRow[], index: number): void {
  while (index > 0) {
    const parent = (index - 1) >> 1;
    if (!sampleLess(heap[parent]!, heap[index]!)) break;
    [heap[parent], heap[index]] = [heap[index]!, heap[parent]!];
    index = parent;
  }
}

function sampleSiftDown(heap: SampleRow[], index: number): void {
  for (;;) {
    const left = index * 2 + 1;
    if (left >= heap.length) return;
    const right = left + 1;
    let largest = left;
    if (right < heap.length && sampleLess(heap[left]!, heap[right]!)) largest = right;
    if (!sampleLess(heap[index]!, heap[largest]!)) return;
    [heap[index], heap[largest]] = [heap[largest]!, heap[index]!];
    index = largest;
  }
}

function retainSample(heap: SampleRow[], row: SampleRow): void {
  if (heap.length < STATISTICS_SAMPLE_ROWS) {
    heap.push(row);
    sampleSiftUp(heap, heap.length - 1);
  } else if (sampleLess(row, heap[0]!)) {
    heap[0] = row;
    sampleSiftDown(heap, 0);
  }
}

function uint64SiftUp(heap: bigint[], index: number): void {
  while (index > 0) {
    const parent = (index - 1) >> 1;
    if (heap[parent]! >= heap[index]!) break;
    [heap[parent], heap[index]] = [heap[index]!, heap[parent]!];
    index = parent;
  }
}

function uint64SiftDown(heap: bigint[], index: number): void {
  for (;;) {
    const left = index * 2 + 1;
    if (left >= heap.length) return;
    const right = left + 1;
    const largest = right < heap.length && heap[right]! > heap[left]! ? right : left;
    if (heap[index]! >= heap[largest]!) return;
    [heap[index], heap[largest]] = [heap[largest]!, heap[index]!];
    index = largest;
  }
}

function retainKmv(heap: bigint[], seen: Set<bigint>, hash: bigint): void {
  if (seen.has(hash)) return;
  if (heap.length < STATISTICS_KMV_HASHES) {
    heap.push(hash);
    seen.add(hash);
    uint64SiftUp(heap, heap.length - 1);
  } else if (hash < heap[0]!) {
    seen.delete(heap[0]!);
    heap[0] = hash;
    seen.add(hash);
    uint64SiftDown(heap, 0);
  }
}

function kmvCount(heap: bigint[], nonnullRows: bigint): bigint {
  if (heap.length < STATISTICS_KMV_HASHES) return BigInt(heap.length);
  const numerator = BigInt(STATISTICS_KMV_HASHES - 1) << 64n;
  const denominator = heap[0]! + 1n;
  let estimate = (numerator + denominator - 1n) / denominator;
  if (estimate < BigInt(STATISTICS_KMV_HASHES + 1)) estimate = BigInt(STATISTICS_KMV_HASHES + 1);
  return estimate > nonnullRows ? nonnullRows : estimate;
}

// TermCounter is PostgreSQL's lossy counting over GIN term occurrences (statistics.md §4.5), bucket
// width STATISTICS_TERM_BUCKET_WIDTH. Entries are keyed by the term's byte-exact latin1 string;
// removal is a per-entry predicate and finish sorts by (count, bytes), so the Map's iteration order
// never reaches the result.
class TermCounter {
  occurrences = 0n;
  entries = new Map<string, { term: Uint8Array; count: bigint; delta: bigint }>();
  pruned = false;
  oversized = false;

  add(term: Uint8Array): void {
    if (this.occurrences < I64_MAX) this.occurrences++;
    const width = BigInt(STATISTICS_TERM_BUCKET_WIDTH);
    const bucket = (this.occurrences + width - 1n) / width;
    if (term.length > STATISTICS_MAX_VALUE_BYTES) {
      this.oversized = true;
    } else {
      const key = byteKey(term);
      const entry = this.entries.get(key);
      if (entry !== undefined) entry.count++;
      else this.entries.set(key, { term: term.slice(), count: 1n, delta: bucket - 1n });
    }
    if (this.occurrences % width === 0n) {
      for (const [key, entry] of this.entries) {
        if (entry.count + entry.delta <= bucket) {
          this.entries.delete(key);
          this.pruned = true;
        }
      }
    }
  }

  finish(): TermStatistics {
    const ranked: [Uint8Array, bigint][] = [...this.entries.values()]
      .map((entry): [Uint8Array, bigint] => [entry.term, entry.count])
      .sort((a, b) => (a[1] > b[1] ? -1 : a[1] < b[1] ? 1 : compareBytes(a[0], b[0])));
    let untracked = 0n;
    const next = ranked[STATISTICS_TERM_ENTRIES];
    if (next !== undefined && next[1] > untracked) untracked = next[1];
    if (this.pruned) {
      const width = BigInt(STATISTICS_TERM_BUCKET_WIDTH);
      const buckets = this.occurrences / width + (this.occurrences % width !== 0n ? 1n : 0n);
      if (buckets > untracked) untracked = buckets;
    }
    if (this.oversized && untracked < 1n) untracked = 1n;
    return {
      occurrences: this.occurrences,
      untrackedRows: untracked,
      entries: ranked.slice(0, STATISTICS_TERM_ENTRIES),
    };
  }
}

type Group = { value: StatisticsValue; frequency: number };

function finishDistribution(
  sample: SampleRow[],
  analyzedRows: bigint,
  distinctCount: bigint,
): { sampleNonNullRows: number; mcv: StatisticsMcv[]; histogram: StatisticsValue[] } {
  const sampleNonNullRows = sample.filter((row) => row.nonnull).length;
  if (sampleNonNullRows === 0) return { sampleNonNullRows: 0, mcv: [], histogram: [] };
  const hasOversized = sample.some((row) => row.nonnull && row.oversized);
  const retained = sample
    .flatMap((row) => (row.retained === null ? [] : [row.retained]))
    .sort((a, b) => compareBytes(a.key, b.key));
  const groups: Group[] = [];
  for (const value of retained) {
    const last = groups[groups.length - 1];
    if (last !== undefined && compareBytes(last.value.key, value.key) === 0) last.frequency++;
    else groups.push({ value, frequency: 1 });
  }
  const allGroups =
    analyzedRows <= BigInt(STATISTICS_SAMPLE_ROWS) &&
    !hasOversized &&
    groups.length <= STATISTICS_MCV_ENTRIES;
  const selected = groups
    .filter(
      (group) =>
        allGroups ||
        (group.frequency >= 2 &&
          BigInt(group.frequency) * distinctCount > BigInt(sampleNonNullRows)),
    )
    .sort((a, b) => b.frequency - a.frequency || compareBytes(a.value.key, b.value.key))
    .slice(0, STATISTICS_MCV_ENTRIES);
  const selectedKeys = new Set(selected.map((group) => byteKey(group.value.key)));
  const mcv = selected.map((group) => ({ value: group.value, frequency: group.frequency }));
  if (hasOversized) return { sampleNonNullRows, mcv, histogram: [] };
  const remaining: StatisticsValue[] = [];
  for (const group of groups) {
    if (selectedKeys.has(byteKey(group.value.key))) continue;
    for (let i = 0; i < group.frequency; i++) remaining.push(group.value);
  }
  if (remaining.length < 2) return { sampleNonNullRows, mcv, histogram: [] };
  const count = Math.min(STATISTICS_HISTOGRAM_BOUNDS, remaining.length);
  const histogram = Array.from({ length: count }, (_, i) => {
    const rank = Math.floor((i * (remaining.length - 1)) / (count - 1));
    return remaining[rank]!;
  });
  return { sampleNonNullRows, mcv, histogram };
}

function byteKey(bytes: Uint8Array): string {
  let out = "";
  for (const byte of bytes) out += String.fromCharCode(byte);
  return out;
}

// collectColumnStatisticsSteps scans one column for ANALYZE as a continuation, so each leaf fault and
// overflow read can await an asynchronous page codec (ts-runtime.md §11.1).
export function* collectColumnStatisticsSteps(
  table: Table,
  store: TableStore,
  collations: (Collation | null)[],
  column: number,
  meter: Meter,
): Steps<ColumnStatistics> {
  meter.charge(COSTS.pageRead * BigInt(store.nodeCount()));
  const columnType = table.columns[column]!.type;
  const eligible = distributionEligible(columnType);
  const termCounter = termEligible(columnType) ? new TermCounter() : null;
  const sample: SampleRow[] = [];
  const kmv: bigint[] = [];
  const kmvSeen = new Set<bigint>();
  let analyzedRows = 0n;
  let nullCount = 0n;
  let widthSum = 0n;
  const mask = table.columns.map((_, i) => i === column);
  const rows = store.scanStepsIter(unboundedBound(), false);
  let drained = false;
  try {
    for (;;) {
      const step = yield* rows.nextSteps();
      if (step.done) {
        drained = true;
        break;
      }
      const [storageKey, stored] = step.value;
      meter.guard();
      const units = store.statisticsScanUnits(storageKey, stored, column);
      meter.charge(
        COSTS.pageRead * BigInt(units.pages) +
          COSTS.valueDecompress * BigInt(units.decompress) +
          COSTS.storageRowRead,
      );
      const row = yield* store.resolveColumnsSteps(stored, mask);
      const value = row[column]!;
      const priority = fnv1a64(storageKey);
      const ordinal = analyzedRows;
      if (analyzedRows < I64_MAX) analyzedRows++;
      if (value.kind === "null") {
        if (nullCount < I64_MAX) nullCount++;
        meter.charge(COSTS.statisticsValue);
        retainSample(sample, {
          priority,
          ordinal,
          nonnull: false,
          oversized: false,
          retained: null,
        });
        continue;
      }
      const bodyLength = encodeValue(store.columnTypes()[column]!, value).length - 1;
      const key = eligible
        ? encodeTypedKey(table.columns[column]!.type, value, collations[column]!)
        : null;
      const width = key === null ? bodyLength : key.length;
      widthSum += BigInt(width);
      if (widthSum > I64_MAX) widthSum = I64_MAX;
      meter.charge(COSTS.statisticsValue * BigInt(Math.max(1, width)));
      if (key !== null) retainKmv(kmv, kmvSeen, fnv1a64(key));
      if (termCounter !== null) {
        for (const term of ginValueTerms(columnType, value)) {
          meter.charge(COSTS.statisticsValue * BigInt(Math.max(1, term.length)));
          termCounter.add(term);
        }
      }
      const oversized =
        bodyLength > STATISTICS_MAX_VALUE_BYTES ||
        (key !== null && key.length > STATISTICS_MAX_VALUE_BYTES);
      retainSample(sample, {
        priority,
        ordinal,
        nonnull: true,
        oversized,
        retained: key !== null && !oversized ? { value, key: key.slice() } : null,
      });
    }
  } finally {
    if (!drained && rows.returnSteps !== undefined) yield* rows.returnSteps();
  }
  meter.guard();
  const nonnullRows = analyzedRows - nullCount;
  const distinctCount = eligible ? kmvCount(kmv, nonnullRows) : null;
  const distribution =
    distinctCount === null
      ? {
          sampleNonNullRows: sample.filter((row) => row.nonnull).length,
          mcv: [],
          histogram: [],
        }
      : finishDistribution(sample, analyzedRows, distinctCount);
  return {
    analyzedRows,
    stale: false,
    nullCount,
    widthSum,
    distinctCount,
    sampleRows: sample.length,
    ...distribution,
    terms: termCounter === null ? null : termCounter.finish(),
  };
}
