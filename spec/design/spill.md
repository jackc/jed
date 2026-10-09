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

`ORDER BY` uses an external merge sort ([§4](#4-external-merge-sort-the-order-by-operator))
and a streaming scan feed ([§5](#5-streaming-the-input-the-single-table-feed)). Hash JOIN,
aggregation and DISTINCT use bounded repeatable row spools and ordered disk hash
partitions ([§7](#7-blocking-hash-operators-ordered-partition-replay)). Input spools,
hash state, aggregate collections, and output order are separate bounded owners.

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
- **The budget bounds one operator, deterministically by the logical size schedule.** A row's
  resident memory is its `row_bytes` from the shared query-memory schedule
  ([memory.md](memory.md) §3/§6.6 — a fixed base per row and value plus the variable payload:
  text/bytea length, decimal digit groups, container elements), a state-map entry
  `ENTRY + key + value`. It is not the exact heap footprint; spill *timing* stays invisible to
  results and cost ([§6](#6-determinism--cost-invariance)), but because every core measures the
  same logical bytes, every core spills at the same input row — which keeps the query-memory
  charge of spill structures (charged on the same measure) cross-core identical. Rows enter a
  sorter or spill structure with their untouched slots NULLed. The default budget
  (`DEFAULT_WORK_MEM = 256 MiB`, matching the buffer-pool default) is sized so a RAM-sized sort
  stays fully in memory; a host bounds a hostile/large sort by lowering it.

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
3. Compact adjacent runs at binary size levels as they are produced. Equal-level runs
   merge with a two-source reader, preserving their original input order. At most one
   run per cardinality bit remains (64 in native cores; 53 for TS exact numbers),
   bounding both run metadata and final open descriptors independently of row count.
4. At `finish`, if no run ever spilled, just stable-sort the buffer in memory and return it (the
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
record format (which is schema-driven and a cross-core contract). The run files live in a host
scratch target **independent of the database path** — the native file hosts default to the OS temp
directory, never beside the database — so a read-only open continues to work when the database's
filesystem is read-only ([api.md](api.md) §2.1). Scratch files are created exclusively with private
permissions, via stdlib file I/O only (no new dependency — CLAUDE.md §14; memory-safe, no
`unsafe`/cgo — §13), and are deleted as the merge drains each one. Failure to create or extend a
run is `58030 io_error`; the sorter never evades `work_mem` by retaining the overflow in memory.
`wasm32-wasip1` is the host exception: WASI preopens provide database-file access, but Rust `std`
has no OS temp-directory implementation for that target. Until the spill-target override lands,
the WASI wrapper supplies no scratch target and its sorts remain resident, like the OPFS host.

## 5. Streaming the input (the single-table feed)

Bounding the *sort* is only half the win: if the executor first materializes every scanned/filtered
row into a `Vec` and *then* feeds the sorter, the input copy is still unbounded. So for the case
where the input is a single relation — **single table, no join, non-aggregate, non-`DISTINCT`, with
an `ORDER BY`** — the executor **fuses** scan → filter → `Sorter.push` directly: a row is scanned,
its touched columns resolved, the `WHERE` applied, and a survivor pushed into the sorter, which
spills as it fills. The full input is **never** resident; peak memory is one run plus the merge
heap.

For a **join / multi-table** `ORDER BY`, the bounded hash pipeline in §7 spools
intermediate rows before sorting; remaining materialized producers retain their source
contract and transfer their rows into downstream spill storage. Either way `finish` yields the sorted rows, which are then windowed
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

## 7. Blocking hash operators: ordered partition replay

The spill implementation must bound **all operator-owned input and state**, not merely
the hash directory. A build table backed by a resident copy of every source row is not
a bounded join. The same rule applies to post-WHERE rows, grouping keys, dedup keys,
and intermediate join output. A repeatable row spool keeps a budgeted resident prefix,
then writes source-order rows to private scratch storage. Reading or replaying scratch
does not charge storage/page/row costs again.

### JOIN

Partition the build side by its canonical hash, retaining build order within each
partition. Probe in the selected plan's original probe order. A fitting partition may
use a resident hash table; an oversized or skewed partition must be processed with a
bounded record-at-a-time fallback, never loaded wholesale. Full hash collisions still
compare the complete canonical key. NULL keys never match. Candidate enumeration is
a stream, not a vector proportional to the number of duplicate matches.

This is an ordered grace-hash variant: replay the appropriate build partition for
each original-order probe instead of emitting partition-order join results. It trades
additional unmetered scratch I/O for preserving the established probe/bucket sequence,
residual-ON evaluation, outer-join null extension, and LIMIT behavior. Repartitioning
must terminate even for one hot key; bounded replay is the terminal skew fallback.

### GROUP BY and DISTINCT

Aggregation processes grouping sets, input rows, and aggregate operands in their
existing order. A disk-backed keyed state store evicts accumulator state rather than
reordering the fold by hash partition. This is necessary because decimal work, integer
overflow, FILTER short-circuiting, and failures in other groups are observable. A
group retains its first-occurrence ordinal; finalization follows that ordinal and
grouping-set order. Empty grouping sets retain their pre-created grand-total group.

DISTINCT uses the same partitioned exact-key lookup discipline, retaining the first
occurrence. Aggregate DISTINCT includes the group and aggregate ordinal in the key;
duplicates and NULLs skip the fold while retaining the existing operand/accumulate
charges. Disk hash equality must use value-canonical keys, including decimal scale,
floating zero/NaN, NULL, and recursive container equality.

Finite accumulator state and growing accumulator collections are different owners.
Spilling a group directory does not bound a single group's ordered-set, hypothetical,
JSON, or string/array state. Such collections need their own replayable scratch or
external ordering. A final scalar value remains subject to scalar admission, and a
materializing host API's collected rows to the query-memory account
([memory.md](memory.md) §5); `work_mem` is not a whole-query or process-RSS ceiling.

### Resource and correctness gates

- Every spool, partition, and output sort has a bounded resident buffer. Metadata and
  open descriptors must not grow once per row or run: use a fixed partition directory
  and bounded merge fan-in. The working bound allows a current record and fixed I/O
  buffers in addition to `work_mem`; one large value is not split by this threshold.
- Scratch uses the existing host target and private exclusive creation. All normal,
  early-LIMIT, evaluator-error, cost-abort, and I/O-error paths release scratch.
  Creation/read/write failure is `58030`, never permission to fall back to resident data.
- Scan, expression evaluation, build/probe, and fold work is charged exactly once at its
  original logical position. Scratch reads, writes, key lookup, and state eviction add
  no new cost units. Per-node EXPLAIN actual costs remain unchanged.
- The same SQL corpus runs in ordinary and forced-spill file modes, asserting rows,
  types, errors, and existing costs. Internal tests additionally prove actual spill,
  peak retained state, skew/collision handling, and cleanup; equal rows alone are not
  evidence of a bounded operator.
- Larger-than-work-memory benchmarks stream their answers and record checksums,
  elapsed time, budget, input size, and process peak RSS. They do not describe a small
  forced-spill fixture as evidence that a dataset exceeds physical machine RAM.

## 8. Slicing & follow-ons

Sequenced so the canonical operator lands first on a frozen budget seam:

- **External merge sort for `ORDER BY` ✅ (this slice).** The `Sorter`, the spill-run files, the
  streaming single-table feed, and the `work_mem` API. Built Rust-first, then Go/TS — a
  result/cost-neutral change, so each core lands green independently (like P5.1 / P6.4b).
- **Bounded `ORDER BY ... LIMIT` top-k ✅.** The stable max-heap runs before spill when fixed-width K
  fits `work_mem`; otherwise the external sorter remains authoritative. Expression/collation error
  timing, LIMIT 0, overflow, and the excluded blocking shapes are corpus-pinned; per-core tests assert
  both the no-run and fallback-to-run paths.

- **Hash JOIN, aggregate and DISTINCT spill ✅.** Ordered partition replay (§7),
  bounded input/intermediate/output spools, external ordering and finite descriptor
  counts. Hash probes retain full-key collision checks and original candidate order.
  Group state preserves global fold order; aggregate DISTINCT membership spills too.
  Ordered-set and hypothetical collections use scratch and bounded replay/sorting;
  JSON aggregates assemble their final scalar after replay. Direct scans and their
  cost prepasses stream. The shared forced-spill corpus joins `mise run test`/`mise run ci`;
  `mise run bench:spill` measures wide inputs larger than `work_mem`.

Remaining allocation owners are explicit. Upstream materialized CTE/derived/SRF/index
producers, the window stage's buffer, and materialized host results are row buffers
charged by the query-memory account (`max_query_memory_bytes`, [memory.md](memory.md)
§5, slice Q1). Operator state — spools, state maps, hash row tables, sorter runs, top-k
heaps, accumulator collections — is charged by the same account (slice Q2,
[memory.md](memory.md) §6): each structure charges its resident elements and releases them
as it spills, so a spilling operator's charge stays within `work_mem` plus one element.
Rows a materialized relation hands to a spool leave the row account and enter the spool's
charge. Pending writes (slice Q3) remain outside the account.

A later refinement, also not foreclosed: routing the spill files through a host **storage seam**
abstraction (storage.md §2) so the browser/OPFS host spills too, rather than the direct stdlib
temp-file I/O this slice uses.
