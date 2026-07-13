# Streaming + spill-to-disk operators — design

> The reasoning behind bounding a blocking operator's memory and **spilling to disk** when it
> is exceeded (CLAUDE.md §9/§13), so a query over larger-than-RAM data never materializes its
> whole input/output in memory. This is a *design* doc; the work-memory budget API is
> [api.md](api.md) §2.1, the cost contract is [cost.md](cost.md), and the storage seam the
> spill files use is [storage.md](storage.md) §2. When a decision here changes, update
> [CLAUDE.md](../../CLAUDE.md) §9 and [storage.md](storage.md) §6 in the same edit.

This is the **Phase 6 "Streaming + spill-to-disk operators"** item (TODO.md). It depends on
the paged storage / bounded buffer pool ([pager.md](pager.md), P6.4): the pool already bounds
the *storage* residency (a table's pages are a cache, not the whole file), but the **executor**
still copies every scanned row into an in-memory `Vec`/slice and sorts/dedups/aggregates it
there — a second, unbounded materialization in the executor's heap, above the storage seam,
exactly the "operator that requires its whole input in RAM" the §1 binding rule forbids. This
item removes that materialization for the blocking operators, one operator at a time.

## 1. What this slice changes, and why

The blocking operators are `ORDER BY` (sort), `GROUP BY`/aggregate (hash aggregate), `DISTINCT`
(hash dedup), and a hash `JOIN`. Each must, in principle, see more than one row at once, so each
is a candidate to bound by a **work-memory budget** and spill the overflow to disk.

**This slice lands the first and most canonical one: `ORDER BY` via an external merge sort**
([§4](#4-external-merge-sort-the-order-by-operator)), plus the **streaming scan→sort feed** that
keeps a single-table `ORDER BY` from materializing its input at all
([§5](#5-streaming-the-input-the-single-table-feed)). The other three are sequenced as
follow-ons ([§7](#7-slicing--follow-ons)) because each needs a *different* algorithm (a spilling
hash table, or — for hash JOIN — a hash-join operator that does not yet exist; jed joins are
nested-loop today).

The blocking sort also has a results-identical **bounded top-k** rule for `ORDER BY ... LIMIT`
([§4.1](#41-bounded-top-k-before-spill)): finite windows that fit the budget avoid creating runs.

## 2. The work-memory budget

A **`work_mem`** handle setting (api.md §2.1, [§3](#3-the-budget-api)) bounds the memory a single
blocking operator may hold resident before it spills — PostgreSQL's `work_mem`, and stated in the
same unit (**bytes**). It is a *handle* setting, not stored in the file and not a §8 byte
contract: it changes **when** an operator spills, never **what** the query observes.

Two scope refinements, both mirroring the buffer pool ([pager.md](pager.md) §1):

- **In-memory databases never spill.** A database with no backing file has nowhere to spill
  *to* (a query against it touches RAM either way), so it keeps the blocking operator fully
  resident regardless of `work_mem` — exactly as the buffer pool keeps an in-memory database's
  tree resident. The spill path is for **file-backed** databases. (The conformance harness's
  default in-memory handle therefore never spills, which is why this whole subsystem is
  result/cost-invariant for the corpus — [§6](#6-determinism--cost-invariance).)
- **The budget bounds one operator, deterministically by a cheap estimate.** A row's memory is
  estimated by a per-value size estimate (a fixed base per value plus its variable payload —
  text/bytea length, decimal digit groups), summed over the row. The estimate need not be the
  exact heap footprint: it only decides *spill timing*, which is invisible to results and cost
  ([§6](#6-determinism--cost-invariance)), so a cheap deterministic estimate is enough. The
  default budget (`DEFAULT_WORK_MEM = 256 MiB`, matching the buffer-pool default) is sized so a
  RAM-sized sort stays fully in memory; a host bounds a hostile/large sort by lowering it.

## 3. The budget API

`work_mem` is plumbed exactly like the buffer-pool `cache_bytes` and the `max_cost` ceiling
(api.md §2.1/§8): an **open-time** option plus a setter, a handle setting that the executor reads.

- `open(path, { cache_bytes / work_mem })` carries it (Rust `OpenOptions { work_mem }` / Go
  `OpenOptions { WorkMem }` / TS `{ workMem }`), default `DEFAULT_WORK_MEM`. As an **option**,
  `work_mem = 0` (or unset) means **the default** budget, *not* unlimited — the zero value stays a
  safe finite budget; the unbounded/never-spill mode is reached only via the setter below (uniform
  across cores, api.md §2.1).
- `db.set_work_mem(bytes)` / `SetWorkMem` / `setWorkMem` sets it on an open handle (the test hook,
  and the runtime knob), mirroring `set_max_cost`. Here `0` means **unlimited** (never spill — the
  whole operator stays resident, the pre-spill behavior).
- It is **not** a create-time parameter (unlike `page_size`): it belongs to the caller's memory,
  so any handle on the file may choose its own, like `cache_bytes` and `max_cost`.

## 4. External merge sort (the `ORDER BY` operator)

A `Sorter` replaces the in-memory `sort_by`/`SliceStable` at the plain (non-aggregate,
non-`DISTINCT`) `ORDER BY` site. It bounds its memory to `work_mem` by the textbook **external
merge sort**:

1. **Accumulate** pushed rows into an in-memory run buffer, tracking its estimated bytes.
2. When the buffer exceeds `work_mem` (and the database is file-backed), **stable-sort** the
   buffer by the order keys and **spill** it as one **sorted run** to a temporary file, then
   clear the buffer. Repeat. Each run is internally sorted; runs are produced in input order
   (run 0 is the first chunk of input, run 1 the next, …).
3. At `finish`, if no run ever spilled, just stable-sort the buffer in memory and return it (the
   unchanged fast path — the dominant RAM-sized case). Otherwise stable-sort the final partial
   buffer and **k-way merge** all runs + that buffer with a min-heap, emitting rows in sorted
   order without ever holding more than one row per run plus the heap.

The merge **reproduces the single in-memory stable sort byte-for-byte**
([§6](#6-determinism--cost-invariance)).

### 4.1 Bounded top-k before spill

A plain SELECT with a blocking `ORDER BY` and constant `LIMIT` retains only
`K = OFFSET + LIMIT` rows in a max-heap, then sorts those retained rows for output. `LIMIT 0`
uses K=0 regardless of OFFSET; checked i64 addition means K overflow simply keeps the full sorter.
The heap comparator is the exact ORDER BY comparator plus the row's monotonically increasing input
position, so a full key tie retains precisely the stable full-sort order.

The all-C, column-key single-table feed can push directly into this heap. On a file-backed database,
the direct lane is admitted only when every **touched** column is a fixed scalar; untouched slots
are replaced by NULL in a private retained-row copy so their variable payloads are released, and the
cross-core logical estimate `K × (8 + 40 × column_count)` must fit `work_mem`. Touched variable/open
rows and an oversized K
fall back to the existing external `Sorter`. In-memory and runtime-unlimited (`work_mem = 0`) handles
always use top-k. This conservative pre-check is necessary: after a heap has discarded a row, it
cannot reconstruct the full input to begin an ordinary external sort.

Expression ORDER BY values and collated sort keys retain their former failure timing. Expression
keys are materialized for every post-filter row before selection; collated paths first complete the
scan/filter materialization and then decorate every row in input order. Only then does top-k discard
rows. A collated LIMIT 0 still decorates every row and can raise the same sort-key error. The generic
eager plain-SELECT seam applies the same selection to joins, SRFs, CTEs, derived relations, and
non-streamable access paths. DISTINCT, aggregate/group, window, and set-operation sorts stay full.

**The spill file is per-core and internal.** Because spill is not a §8 byte contract (results +
cost are invariant — [§6](#6-determinism--cost-invariance)), the run file's bytes need only
round-trip **within one core, during one query, while the database file is unchanged**. So each
core serializes a run idiomatically with a **self-describing row codec** (a per-value type tag +
payload, plus an opaque pass-through for an untouched [large-values.md](large-values.md) §14
`Unfetched` reference, which rides along to the output and is never read) — *not* the §8 on-disk
record format (which is schema-driven and a cross-core contract). The run files live in the host
temp directory via stdlib file I/O only (no new dependency — CLAUDE.md §14; memory-safe, no
`unsafe`/cgo — §13) and are deleted as the merge drains each one.

## 5. Streaming the input (the single-table feed)

Bounding the *sort* is only half the win: if the executor first materializes every scanned/filtered
row into a `Vec` and *then* feeds the sorter, the input copy is still unbounded. So for the case
where the input is a single relation — **single table, no join, non-aggregate, non-`DISTINCT`, with
an `ORDER BY`** — the executor **fuses** scan → filter → `Sorter.push` directly: a row is scanned,
its touched columns resolved, the `WHERE` applied, and a survivor pushed into the sorter, which
spills as it fills. The full input is **never** resident; peak memory is one run plus the merge
heap.

For a **join / multi-table** `ORDER BY`, the existing materialize → nested-loop → `WHERE` pipeline
runs unchanged (the join itself materializes its base tables — bounding *that* is the deferred hash
JOIN item, [§7](#7-slicing--follow-ons)), then the filtered rows drain into the same `Sorter`, which
still bounds the sort. Either way `finish` yields the sorted rows, which are then windowed
(`LIMIT`/`OFFSET`) and projected by streaming the merge — the output is not re-materialized either
(the `OFFSET` clamp uses the sorter's known total row count, not a materialized length).

## 6. Determinism & cost invariance

This is the load-bearing simplification, identical in spirit to the buffer pool's
([pager.md](pager.md) §3/§5): **spill changes timing, never observation.**

- **Byte-identical results.** The k-way merge reproduces the in-memory stable sort exactly. The
  in-memory sort is stable: equal-key rows keep input order. In the external sort each run is a
  *contiguous input-order chunk*, stably sorted, and the merge breaks key ties by **(run index,
  position within run)** — and since run 0 holds the earliest input positions, run 1 the next, …,
  and the final in-memory buffer the latest, that tie-break is exactly input order. So the merged
  sequence equals the stable sort's, row for row, regardless of how many times it spilled. The
  result is invariant to `work_mem`, the spill count, and the database's file-vs-memory backing.
- **Byte-identical cost.** The `ORDER BY` sort is **unmetered** (cost.md §3 "What is NOT
  metered"), and spill adds only sort-internal I/O, which is likewise unmetered. The streaming
  feed ([§5](#5-streaming-the-input-the-single-table-feed)) scans, filters, and produces exactly
  the rows the eager path did, charging the same `page_read` block, `storage_row_read` per
  scanned row, filter `operator_eval`, and `row_produced` per windowed row — so the accrued
  **total is unchanged**, and every `# cost:` corpus value holds. (The fused feed *interleaves*
  scan and filter accrual where the eager path charged them in two phases; this changes neither
  the total nor any result, only the *instant* at which accrued cost would cross a `max_cost`
  ceiling **if a filter trapped mid-scan** — an unobservable detail on a trapping statement,
  made cross-core-identical by mirroring the fused loop in all three cores. No corpus or per-core
  test pins it.)
- **Not a §8 byte contract.** Like the buffer pool and P5.3's concurrency, the sorter, the spill
  format, and the merge are **internal performance machinery**: each core implements them
  idiomatically, the only contract being that results and cost stay byte-identical. So no golden
  fixture, no new cost unit, and no new on-disk `format_version` — the database file is untouched.
- **No nondeterminism leaks.** The merge orders by the order keys + the deterministic (run,
  position) tie-break, never by hashmap iteration or spill-file path; the spill I/O is unmetered,
  so timing never enters cost (CLAUDE.md §8/§10).

## 7. Slicing & follow-ons

Sequenced so the canonical operator lands first on a frozen budget seam:

- **External merge sort for `ORDER BY` ✅ (this slice).** The `Sorter`, the spill-run files, the
  streaming single-table feed, and the `work_mem` API. Built Rust-first, then Go/TS — a
  result/cost-neutral change, so each core lands green independently (like P5.1 / P6.4b).
- **Bounded `ORDER BY ... LIMIT` top-k ✅.** The stable max-heap runs before spill when fixed-width K
  fits `work_mem`; otherwise the external sorter remains authoritative. Expression/collation error
  timing, LIMIT 0, overflow, and the excluded blocking shapes are corpus-pinned; per-core tests assert
  both the no-run and fallback-to-run paths.

Deferred follow-ons (none foreclosed; each its own slice with the same invariance contract):

- **Spilling hash aggregate (`GROUP BY` / aggregate).** Bound the group hash table by `work_mem`,
  spilling partitions when exceeded (a grace-style partitioned aggregation that preserves the
  first-occurrence group order the in-memory path emits). The aggregate path's group rows are
  already *reduced* data (one row per group), so today's in-memory sort of the group rows stays —
  bounding the *group table* is this follow-on.
- **Spilling `DISTINCT`.** Same shape: bound the dedup set, spill partitions, preserve
  first-occurrence order. (A sort-based dedup would change that order, so it must be a partitioned
  hash, not the merge sort above.)
- **Hash `JOIN` + grace-hash spill.** jed joins are nested-loop today; this first adds a hash-join
  operator (a planner choice), then bounds its build side by `work_mem` with grace-hash
  partitioning. This is the item that bounds a *join's* input materialization
  ([§5](#5-streaming-the-input-the-single-table-feed)).

A later refinement, also not foreclosed: routing the spill files through a host **storage seam**
abstraction (storage.md §2) so the browser/OPFS host spills too, rather than the direct stdlib
temp-file I/O this slice uses.
