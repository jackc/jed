# Memory admission

Cost is a deterministic work allowance, not a memory limit. A large value can
allocate substantial memory before another row or expression is visited, and a
join can copy one large value into many rows for a handful of cost units. Memory
admission therefore runs **independently of cost**, through two per-statement
accounts and one database-owned account (§8):

| Account | Setting | Default | Model | Error |
|---|---|---|---|---|
| Scalar allocation | `max_scalar_bytes` | 64 MiB | cumulative churn, never refunded | `54P04` |
| Query memory | `max_query_memory_bytes` | unlimited | live logical bytes, released when dropped; opens holding the transaction's pending writes (§7) | `54P05` |
| Committed storage (§8) | `max_storage_bytes` (database) | unlimited | an in-memory domain's `page_count × page_size`, checked at commit | `54P06` |

All are **guardrails, not heap caps.** They bound logical bytes computed from a
shared, representation-independent schedule (or, for committed storage, the
logical page count), so an abort is deterministic and
cross-core identical ([determinism.md](determinism.md) §3: a limit hit is part of
G1/G2, not a ledger exception). None equals process RSS, and all allow bounded
overshoot: a row may be constructed before it is admitted, and a base relation's
storage read may decode before its rows are admitted one by one. Hosts bound
deployment memory with these accounts, `work_mem`, `temp_buffers`, the page-pool
budget, and process-level limits together.

## 1. Scalar allocation allowance (implemented)

`max_scalar_bytes` is a host session setting, with a finite **64 MiB** default
from `spec/cost/schedule.toml`. Positive values replace it; zero or negative values
restore the default. It is not persisted, transactional, or SQL-mutable. Session
options and setters expose it in Rust, Go, and TypeScript. Existing sessions and
cursors keep their own settings. An error is `54P04 scalar_memory_limit_exceeded`.

This is a **cumulative logical allocation allowance per statement**, not a live
heap counter or a process RSS ceiling. The covered allocations are:

- Exact output UTF-8 bytes of `repeat`, `lpad`, and `rpad`. Their preflight does
  not allocate character vectors or an output buffer. TS measures UTF-8 without
  first allocating an encoded copy; its runtime may store strings as UTF-16.
- Decimal transcendental working storage: `8 * D` logical bytes for each charged
  kernel step, with D defined in cost.md §8. This conservative reservation is
  representation-independent, not a measurement of the runtime allocator.

A reservation succeeds if `used + bytes <= limit`, implemented by comparing
`bytes <= limit - used` to avoid overflow. Successful reservations accumulate;
there are **no refunds within a statement**, even for temporary values. This
bounds repeated allocation churn as well as a single large result. Input values
are already owned and are not counted again merely for being read.

All internal meters share the statement's allowance: nested expressions, scalar
subqueries, CTE parts, recursive terms, set-operation arms, index-expression and
partial-predicate maintenance, and generated FK actions. Legacy scratch evaluators
still discard their cost under the existing cost schedule, but share memory
admission. A frozen streaming cursor captures the allowance and its limit; opening
another cursor cannot reset it. A new statement gets a fresh allowance, including
a statement after an error. A script gets one allowance per SQL statement.

Strict NULL short-circuiting and unselected CASE branches reserve nothing.
Structural result limits run before output admission. Cost is charged and guarded
before reserving output/scratch bytes; when both gates reject that step, the cost
error wins. An allocation failure is an ordinary SQL statement failure: writes are
rolled back and an explicit transaction enters the failed state.

The allowance does not cover concatenation/replacement, array/JSON construction,
or other scalar kernels yet; those remain scalar-admission follow-ons.

## 2. Query memory account

`max_query_memory_bytes` is a host session setting: Rust `SessionOptions
{ max_query_memory_bytes }` / `set_max_query_memory_bytes`, Go
`SessionOptions.MaxQueryMemoryBytes` / `SetMaxQueryMemoryBytes`, TS
`maxQueryMemoryBytes` / `setMaxQueryMemoryBytes`. A positive value is the live
budget in logical bytes; **zero or negative means unlimited** (the default). It is
not persisted, transactional, or SQL-mutable, so SQL cannot weaken it. Existing
cursors keep the setting they opened with.

The default is unlimited deliberately. The eager executor materializes whole
relations for joins and aggregates over in-memory databases (which never spill),
so any finite default would reject queries over RAM-sized data that run today. A
host serving untrusted SQL opts in, exactly as it opts in to `max_cost`
([session.md](session.md) §5). When the limit is unlimited, no core computes a
size or touches the account: the accounting is free on the default path.

The account is **live**: a reservation adds bytes, a release returns them. A
reservation succeeds if `used + bytes <= limit` (equality allowed, overflow-safe as
in §1); otherwise the statement fails with `54P05 query_memory_limit_exceeded`
("query memory exceeded the limit of N bytes"). Only reservations can fail; a
release never errors and never takes `used` below the balance the account opened
with (zero, or the transaction's pending writes, §7) — a release past it is an
accounting bug, clamped and counted by the conformance runners.

Scope and lifetime follow the scalar allowance: one account per SQL statement,
shared by every internal meter of that statement (subqueries, CTE parts, set-op
arms, recursive terms, generated actions). A statement after an error starts
fresh. A streaming or buffered cursor keeps its statement's account until it is
closed, so rows it still holds stay charged while the host drains it; a later
statement on the same session gets its own account. **Cost wins:** a rejected
reservation consults the statement meter's cost guard before reporting `54P05` —
even at a site whose own charge is not guarded, such as `EXPLAIN`'s per-row charge
— so a step that has already reached `max_cost` or the lifetime budget fails
`54P01`/`54P02` instead. An engine-owned collector whose admission is rejected
likewise consults its cursor's cost guard (for a finished buffered result, the
guard of the statement's final cost). A successful reservation adds no guard
point, and neither does a rejection a spill structure absorbs by spilling (§6.6),
so enabling a budget that never fails cannot change whether, or where, a cost
ceiling aborts.
`54P05` is an ordinary statement failure: writes roll back and an explicit
transaction enters the failed state.

## 3. The logical size schedule

Sizes come from `[memory]` in `spec/cost/schedule.toml`, codegen'd into every
core, and are cross-checked by the shared vectors in
`spec/cost/memory_sizes.toml` (each core evaluates the vector's expression and
asserts its measured bytes). They are representation-independent: they never ask
a runtime how large its objects are.

```
row_bytes(row)   = ROW (32) + Σ value_bytes(v)
value_bytes(v)   = VALUE (32) + payload(v)

payload(NULL, boolean, integers, floats, uuid, date, timestamp,
        timestamptz, interval)           = 0
payload(text | json | jsonpath)          = UTF-8 byte length of the text
payload(bytea)                           = byte length
payload(decimal)                         = DECIMAL_GROUP (4) × base-10⁴ digit groups
                                           of the canonical codec (format.md)
payload(array)                           = ARRAY_DIM (16) × ndim + Σ value_bytes(element)
payload(composite)                       = Σ value_bytes(field)
payload(range)                           = Σ value_bytes(bound) over present bounds
value_bytes(jsonb)                       = jsonb_bytes(root)

jsonb_bytes(node) = VALUE (32) + { string: UTF-8 length; number: DECIMAL_GROUP × groups;
                                   array: Σ jsonb_bytes(element);
                                   object: Σ (UTF-8 length(key) + jsonb_bytes(value));
                                   null/boolean: 0 }
```

`jsonpath` is measured on its canonical normalized text; `json` on its verbatim
text. An unfetched large-value reference ([large-values.md](large-values.md) §14)
measures as a bare `VALUE`.

**Untouched columns.** A pre-projection row carries every column of its
relations, but only the plan's touched set ([cost.md](cost.md) §3) is guaranteed
to be decoded; an untouched slot may hold a decoded copy, a deferred reference,
or a placeholder depending on the leaf's physical state and the core. So a
measurement of a pre-projection row uses the plan's touched mask (the
concatenation of each relation's mask in logical column order): an untouched slot
counts as a bare `VALUE`, a touched slot or an appended slot (window result,
materialized ORDER BY/GROUP BY key) counts its full `value_bytes`. Projected rows
(statement output, derived-table and CTE bodies, subquery results) contain only
computed values and are measured in full. This keeps the measurement a function
of the query and the logical database, never of the leaf state, in-memory versus
file backing, or the core.

## 4. Owners and slices

The account is meant to cover every allocation owner whose size grows with input.
It is filled in by owner class:

| Slice | Owner | Status |
|---|---|---|
| Q1 | **Rows** — row buffers of statement execution and engine result collectors | implemented |
| Q2 | **Operator state** — hash-join tables, group/distinct/dedup sets and keys, aggregate accumulators (`json_agg`/`jsonb_agg` and the other JSON aggregates, ordered-set and hypothetical buffers), sort buffers and top-k heaps, window partition state, columnar lanes, spill spools' resident buffers, recursive-CTE dedup sets | implemented (§6) |
| Q3 | **Pending writes** — the stored bytes a transaction stages, surviving statement boundaries and released at commit/rollback | implemented (§7) |
| Q4 | **Storage** — database-owned: committed in-memory storage under `max_storage_bytes`; page caches stay under `cache_bytes`, evict-only | Q4a implemented (§8) |

Q2 operators that can spill charge only their resident portion and release it as
they spill — when their residency exceeds `work_mem`, and also when the account
rejects a reservation (§6.6). An operator without spill support (or a database with
no scratch target — every in-memory database) fails at the gate instead, as does any
row buffer or value outside a spill structure.

Q3 is necessary because a per-statement account cannot see many small INSERTs
inside `BEGIN`; Q4 because page caches and committed in-memory storage outlive any
statement.

## 5. Q1: rows

A **row buffer** is any engine-held collection of rows that grows with input
during a statement. Q1 charges each row once, when it is first created or copied
into a row buffer, and releases it when the engine discards it. Every core charges
at the same logical sites, in the same order relative to the statement's other
reservations, so the account's balance at every reservation — and therefore the
row at which `54P05` fires — is cross-core identical.

Two measurements are used (§3): a **projected** row (statement output, a
subquery/derived/CTE body result, a group row, a DISTINCT row) is measured in
full; a **pre-projection** row of the eager SELECT pipeline (a relation row, a
join's combined row, a post-WHERE row) is measured under the touched mask — the
relation's own mask for a relation row, the concatenated plan mask for a combined
row. A row is always released under the measurement that reserved it.

### 5.1 Reserve points

`row_bytes` is reserved, after the row is built and immediately before it is
appended, at:

- **Relation materialization** (eager SELECT path): each scanned base-table or
  index-bounded row; each row of `generate_series` and `unnest` as it is generated;
  the whole output of the other set-returning functions (JSON producers,
  `JSON_TABLE`, `jed_*` catalog functions), which are bounded by an existing value
  or the catalog, once produced; each row of a `VALUES` body; each row a
  materialized CTE reference copies from the CTE's buffer; and each prefix row the
  window-top-N lane collects. A derived table or an
  inline CTE body arrives charged as a projected result and is re-measured under the
  relation mask (the difference is released).
- **Joins**: each combined row a join step keeps, each NULL-extended row (LEFT,
  RIGHT, FULL), and — for a costed N-way join — each driver row placed into the
  full-width layout before the first step. This covers the nested-loop, hash,
  index-nested-loop, LATERAL, two-relation and N-way paths and the bounded join
  lanes' collected output.
- **Aggregation**: each synthetic group row, after its aggregates are finalized.
- **DISTINCT**: each first-occurrence projected row kept.
- **Results**: each output row collected by the materialized emission drive (a
  projecting drive, a spool drive, a sorted drive, and a columnar drive), each
  output row a streaming/bounded lane collects into a buffered result (the
  bounded streaming scan, index-order scan, two-table and N-way join top-N), each
  `RETURNING` row, and each `EXPLAIN` output row.
- **Collectors**: each row an engine-owned materializing host API appends while
  draining a cursor — Rust `query_rows` (and `query_map`/`query_row` over it) and
  TypeScript `Statement.all()`. Go has no such helper: its iterators hand each row
  to the host, whose collection is the host's memory. A `54P05` here fails the
  statement and poisons an open block like a mid-drain error.
- **Recursive CTE**: each kept row's copy into the CTE result (the working-table
  row itself transfers), and a copy of the earlier CTEs' buffers taken for the
  recursive term.
- **The FROM-less virtual row**: one bare `ROW` (32 bytes).

`value_bytes` is reserved for each value **appended** to a buffered row:
materialized window keys, window function results, materialized ORDER BY
expression keys, and materialized GROUP BY expression keys. A set operation's
(or a recursive term's) int→decimal coercion reserves the measured growth of the
coerced arm before combining.

### 5.2 Transfers

Moving rows between owners transfers their charge: WHERE survivors, the identity
emission of DISTINCT rows inside the LIMIT/OFFSET window, a subquery's result
returned to its caller, a set-operation arm becoming part of its result, a CTE
body becoming the CTE buffer, a working-table row, and the rows of a buffered
result handed to the host's cursor.

### 5.3 Release points

Releases recompute the released rows' bytes with the measurement that reserved
them:

- **Filters** release each discarded row when it is discarded: a WHERE reject and
  a HAVING reject immediately; the rows outside the LIMIT/OFFSET window when the
  materialized drive emits an identity (DISTINCT) buffer, and the rows a set
  operation's combine or window drops, once that step completes (a lazily driven
  cursor keeps its buffer, and its charge, until it is exhausted). A top-k selection releases its whole input and then
  re-admits the kept rows (never more than it released).
- **Stage hand-off** releases a whole source buffer once the stage that consumed it
  completes: a join step's input rows after its output is built; all materialized
  relations after the join phase (and, for the bounded join lanes, at their end);
  the post-WHERE rows once grouping has emitted every group row; the post-WHERE
  rows after a DISTINCT pass; the group rows after a grouped DISTINCT pass; and the
  projecting emission drive's source buffer after its last output row.
- **Per-row re-materializations**: a LATERAL or index-nested-loop relation's rows
  for one left row, once that left row's join work is done.
- **Consumed subquery results**: a correlated scalar, `EXISTS`, `IN`, or
  quantified subquery's result, once the expression has finished using it.
- **CTE buffers** of a `WITH` (top-level, nested, or data-modifying) once its body
  has run — essential for a nested `WITH` re-run per outer row. A recursive CTE
  releases each working table when the next iteration replaces it, and the copy
  of the earlier buffers at the end. `EXPLAIN ANALYZE` releases the analyzed
  statement's result after counting it.
- **Operator-state hand-off**: rows a materialized relation transfers into a
  bounded spill spool are released as they enter it (spools are Q2 state).
- **Cursor yield**: a buffered result row leaving the engine through a cursor
  (the materialized outcome, a lane's collected result, an identity buffer row, a
  deferred set-operation/`WITH` result) is released as it is yielded, so an
  engine-owned collector re-reserving it nets one charge per row.

Rows retained for the rest of the statement stay charged until it ends: folded
uncorrelated subquery results and the source rows of `INSERT ... SELECT`.

### 5.4 Lanes

Every execution lane follows the same rules for the buffers it actually holds,
and the lane choice is itself deterministic and mirrored across cores — so a
threshold can depend on the lane (and thus on settings such as `max_cost`, which
gates the vectorized lanes, or file versus in-memory backing, which gates the
bounded-spill lane) but never on the core.

- A **streaming lane** that hands each row to the host as it is produced (the pull
  scan) holds no row buffer and reserves nothing for its output; a lazily driven
  projecting, sorted, spool, or columnar emission reserves nothing for rows it
  hands to the host either. A lazily driven projecting buffer stays charged until
  the cursor is exhausted (the materialized drive releases it after its last row),
  so the same statement may have different thresholds through a lazy cursor and
  through the internal materialized path — each mirrored across cores.
- The **columnar and vectorized lanes** charge their group rows and collected
  output as Q1 buffers; their gathered lanes and group tables are operator state
  (§6.2, §6.5).
- The **streaming external sort** lane's input (the sorter's buffer or the
  collated survivor buffer) is sort state (§6.4).
- The **bounded-spill lane** (file-backed aggregation, DISTINCT, and multi-way or
  hash joins) holds its rows in spools (§6.6); its window stage materializes its
  rows as a Q1 buffer while the window runs.

Q1 bounds row-buffer amplification — cross joins, set-returning functions,
recursive CTE output, wide projections of large values, and materialized
results. Operator state is Q2 (§6) and pending writes are Q3 (§7); a single base
relation's storage read before its rows are admitted remains outside the account.

## 6. Q2: operator state

**Operator state** is what an operator holds *besides* its row buffers: the
lookup structures, accumulators, and resident run/spool contents that grow with
its input. Q2 charges it to the same live account, measured with the same
schedule, at sites mirrored in every core — so a `54P05` raised by operator state
is as cross-core identical as one raised by a row buffer. Q2 adds no charge to a
Q1 row buffer and re-measures nothing Q1 already holds: an in-place sort of a
charged buffer, or an index into one, is not separate state.

### 6.1 Measures

```
key_bytes(values)   = Σ value_bytes(v)                (no ROW header)
entry_bytes(values) = ENTRY (32) + key_bytes(values)  one hash/dedup/group entry
```

`ENTRY` is `[memory] entry` in `spec/cost/schedule.toml`. Rows that enter sort
state or a spill structure have their untouched slots (§3) replaced by NULL
first, so they are measured in full with `row_bytes` and the measurement is
again independent of leaf state and core.

### 6.2 Hash, group, and dedup tables

Each is reserved as the entry is inserted and released when its owner completes.

| Owner | Reserve | Release |
|---|---|---|
| Hash-join build table (two-relation, N-way, and bounded-lane eager steps) | `entry_bytes(build key)` per build row whose key has no NULL, after its `hash_build` charge | with the step's input rows, once the step's output is built (§5.3) |
| Group table, per grouping set (eager and vectorized paths) | `entry_bytes(group key)` when a group is created, before the creating row is folded; the whole-table (`()`) group reserves `ENTRY` when pre-created | each group with its accumulator state (below), when the group is finalized — after its synthetic row is built, before that row is admitted |
| `SELECT DISTINCT` dedup set (eager, grouped, and streaming-scan DISTINCT) | `entry_bytes(projected row)` per first occurrence, before the kept row is admitted | when the DISTINCT pass completes (the streaming scan: when the scan ends) |
| Set-operation dedup (`UNION`, `INTERSECT`, `EXCEPT`, distinct and `ALL`) | `entry_bytes(row)` per distinct right-arm row of an `INTERSECT`/`EXCEPT` (its set or count table, built first), then per row a `UNION`/`INTERSECT`/`EXCEPT` (distinct) keeps | when the combine completes |
| Recursive-CTE `UNION` dedup set | `entry_bytes(row)` per kept row, before its result copy is admitted | when the recursive CTE completes |
| Window partition table, per shared partition/sort group | `entry_bytes(partition key)` per partition, when created | when the window stage completes (after its results are appended) |

A **vectorized** group table (the single-integer-key and whole-table lanes) follows
the same rule but finalizes every group before admitting any group row, so it
releases all of its entries after the last group is finalized.

### 6.3 Aggregate accumulators

Accumulators that keep a fixed-size running value (`count`, `sum`, `avg`,
`min`/`max`, the float folds, `bool`/`bit` folds) are covered by their group's
entry. Accumulators that **retain their inputs** reserve each retained input, after
it is folded:

| Accumulator | Per retained input |
|---|---|
| `json_agg`, `jsonb_agg` (and `_strict`, which retains no NULL) | `value_bytes(argument)` |
| `json_object_agg`, `jsonb_object_agg` (and `_unique`, `_strict`) | `value_bytes(key) + value_bytes(value)` |
| `mode`, `percentile_disc`, `percentile_cont` (non-NULL inputs) | `value_bytes(argument)` |
| `rank`, `dense_rank`, `percent_rank`, `cume_dist` (hypothetical-set) | `row_bytes(WITHIN GROUP key tuple)` |
| any `DISTINCT` aggregate's value set | `entry_bytes([argument])` per newly seen non-NULL value |

A grouped accumulator's charge belongs to its group and is released with it
(§6.2). A **window** aggregate's accumulator is released when the accumulator is
discarded: at the end of each partition's running pass, after each row's result
under a re-folded frame (`EXCLUDE`, `FILTER`), and when a moving frame rebuilds
it.

### 6.4 Sort state

The single-table streaming-sort lane holds its survivors as sort state instead of
a row buffer:

- **External sorter.** Each survivor pushed into the sorter reserves its
  `row_bytes`. A run written to scratch releases the rows it held; a survivor whose
  reservation is rejected is written with the run, uncharged (§6.6). The final
  in-memory run stays charged while the sorted output is emitted.
- **Collated survivor buffer** (a collated `ORDER BY`, which sorts in memory):
  each survivor reserves its `row_bytes` as it is collected. A collated top-k
  selection then releases the whole buffer and re-reserves the rows it keeps.
- **Top-k heap.** A survivor the heap retains reserves its `row_bytes`; the row it
  evicts (if any) is released after the newcomer is reserved. A survivor the heap
  rejects, and every survivor under `LIMIT 0`, reserves nothing.

The sorted output's remaining charge is released when its emission completes —
after the last windowed row is collected by the materialized drive, or when a
lazy cursor is exhausted or closed. The bounded-spill lane's sorters (its
`ORDER BY` and its ordered-set finalization) follow the same sorter rule, and
their output transfers into a spool (§6.6). The eager executor's in-place sort of a
charged buffer adds nothing.

### 6.5 Columnar lanes

The columnar projection lane gathers the touched columns of a file-backed relation
into dense lanes in one bulk read. Once the gather completes it reserves
`key_bytes` of every gathered value (the bounded overshoot of a bulk read, like a
base relation's storage read, §5), and releases them when its emission completes
(as in §6.4). The columnar aggregate lane folds during the tree walk without
gathering lanes; it holds only its group table (§6.2).

### 6.6 Spill-capable structures and `work_mem`

The bounded-spill lane (and the file-backed streaming DISTINCT) keeps operator
state in three spill-capable structures. Each measures its **residency with this
schedule** — the same number it charges — so a spill happens at the same input
row in every core:

| Structure | Resident measure per element |
|---|---|
| Row spool | `row_bytes(row)` |
| Keyed state map (groups, dedup sets) | `ENTRY + key_bytes(key) + key_bytes(value)` |
| Hash row table (spilled hash-join build, retained aggregate inputs) | `ENTRY + row_bytes(row)` |

An insert reserves its element's bytes; replacing a resident state-map value
reserves the growth (or releases the shrinkage) of `key_bytes(value)`. A structure
**spills** when its resident bytes then **exceed `work_mem`**, or when **the account
rejects the reservation**: its elements, including the new one, move to scratch and
its whole resident charge is released. A rejected element is never charged, and a
rejection a structure absorbs this way raises no error. A spilled structure reserves
nothing further; a sorter, which starts a new run after writing one, keeps reserving
and spills each run the same way. A structure's remaining charge is released when it is
discarded — a spool or map feeding a stage when that stage completes, and the
lane's final output spool when its emission completes (§6.4).

Because the measures depend on what each element holds, the bounded-spill lane's
**element shapes are part of the contract**:

| Structure | Element |
|---|---|
| Stage spools | relation rows (untouched slots NULL), physically placed driver and combined join rows, NULL-extended rows, post-WHERE rows (the WHERE pass always copies into a new spool, filter or not), group rows, and `ORDER BY` rows decorated with every order-expression value and then every order key (a collated key as `bytea` of its sort key) |
| Group state map | key: the group-key values; value: `[int ordinal, composite(packed) per aggregate]`, packed as `count` → `[int]`, integer `sum` → `[int, bool seen]`, decimal `sum` → `[decimal, bool]`, `avg` → `[decimal, int count]`, float `sum`/`avg` → `[f64, int, bool, bool, bool]`, `min`/`max` → `[value or NULL]`, JSON aggregates → `[bool seen]`, ordered-set and hypothetical → `[]` |
| Group key spool | the group-key values, one row per group in creation order |
| `DISTINCT`-aggregate set / object-agg unique set | `[int ordinal, int aggregate index, value]` / `[int ordinal, int aggregate index, text key]` |
| Retained aggregate inputs (hash rows, keyed by `ordinal × aggregates + index`) | `json_agg` → `[jsonb node]`; `json_object_agg` → `[text key, value]`; ordered-set → `[value]` (`percentile_cont`: `[f64]`); hypothetical → the key tuple |
| Spilled hash-join build | `[bytea encoded key, composite(build row)]` (the key encoding is the §8 byte contract) |
| Probe matches / right-match set | a per-left-row spool of matched build rows / `[int match ordinal]` |
| Finalization | ordered-set sorter rows `[value]` or `[value, bytea collation key]`; percentile spool of sorted rows; `json_object_agg` sorter rows `[int UTF-8 length, text key, jsonb]`; dense-rank distinct set of key tuples |

So a spill-capable operator's charge is bounded by `work_mem` plus one element
and by what the account has left: a budget below `work_mem` makes its operators
spill earlier instead of failing. Only owners that cannot spill — row buffers
(§5), the eager lane's operator state, top-k heaps, collated sort buffers,
columnar lanes — fail `54P05`. Rejection-driven spilling is deterministic for the
same reason `work_mem` spilling is: the account's balance at every reservation is
cross-core identical (§5), so every core rejects, and spills, at the same element.
Each structure spills only itself; it never evicts another owner's charge. The
trigger applies only where `work_mem` spilling does — a file-backed database with a
positive `work_mem`. `work_mem = 0` (never spill) and every in-memory database keep
their structures resident, which charge everything they hold. A deliberately small
budget can therefore turn a query into a long sequence of tiny spills; that costs
scratch I/O and time, but no metered cost (spill.md §6) and no change in results.

### 6.7 Cost wins

A Q2 reservation follows §2: when it is rejected, the statement meter's cost guard
is consulted first, so a step that has already reached `max_cost` or the lifetime
budget reports `54P01`/`54P02`. A structure that reserves with no meter in reach
(a spill structure, sorter, or top-k heap) returns its `54P05` to the operator,
which consults the guard before reporting it — equivalent, since accrued cost only
grows and nothing is charged on the error's way out. The set-operation combine and
the recursive CTE run between their parts' meters and reserve against the account
directly, as their Q1 reservations do.

### 6.8 Not covered

Q2 leaves these transient, input-proportional allocations outside the account:
collation sort-key decorations (computed from already-charged text), a window
frame's cached operand values, merge-heap heads of spilled runs, the index lists
that partition or permute an already-charged buffer, and the per-row key
encodings a hash probe computes and discards. They are bounded by charged state
times a small factor; they are guardrail overshoot, not unbounded growth.

## 7. Q3: pending writes

A write transaction stages its changes in memory until it commits
([transactions.md](transactions.md) §2), and they outlive the statement that made
them, so a per-statement account cannot see them: a block of many small INSERTs
would grow without bound. Q3 charges them to the same budget,
`max_query_memory_bytes`: a session's balance is its transaction's pending writes
plus the current statement's live account.

### 7.1 Measure

Every record version a write stages into a table or an index — by any path:
INSERT, UPDATE, upsert, a foreign-key action, an index build, a table rewrite —
adds its **stored bytes**:

```
stored_bytes(record) = record_size + Σ chain_payload(externalized value)
```

`record_size` is the record's on-disk size ([format.md](../fileformat/format.md)
*Record*, the B+tree's split weight). An externalized value's chain payload is its
raw bytes when stored external-plain and its compressed block when stored
external-compressed ([large-values.md](large-values.md) §2). Each secondary, GIN,
or GiST index entry is a record of its own. Both terms are fixed by the file
format's byte contract, so for a given page size the measure is cross-core
identical and independent of leaf state and of in-memory versus file backing. A
delete stages nothing.

The balance is **cumulative** within a transaction: replacing a row stages a new
version, charged again, though the pending set keeps only the latest. Q3 bounds a
transaction's write volume, not the net residency of its dirty pages. Dirty-page
amplification — a small update decodes a whole leaf — stays outside the account;
it is bounded by the page reads the write performed, which `max_cost` meters.

### 7.2 Lifetime

The bytes belong to the transaction's working snapshots, the main database's and
each attached database's ([attached-databases.md](attached-databases.md) §5), and
travel with them. A rollback, a failed block's COMMIT, or a failed autocommit
statement discards them; a commit releases them. Session-local temp tables are
excluded, since `temp_buffers` bounds them ([temp-tables.md](temp-tables.md) §7).
Bytes are tracked whether or not a budget is active, so a budget set in the middle
of a transaction sees the writes already staged.

### 7.3 Admission

Under a finite budget:

- **Each statement's account opens holding the transaction's pending bytes**, so
  its reservations (§5, §6) must fit beside them.
- **After each successful statement** — every statement inside an explicit block,
  and an autocommit write before it commits — the transaction's pending bytes alone
  must fit the budget, or the statement fails `54P05`. Cost wins (§2): a statement
  that has reached `max_cost` or the lifetime budget reports `54P01`/`54P02`
  instead. The failure is an ordinary statement failure: an autocommit write
  commits nothing, and an explicit block enters the failed state.

Both count toward the statement's peak, so in memory mode a record's peak is still
its minimal passing budget. Like `temp_buffers`, the check runs at the statement
boundary: within one statement, writes are bounded by `max_cost` and by the Q1
buffers that feed them (INSERT … SELECT keeps its source rows charged), and they
are checked once the statement completes. Thresholds are pinned by
`resource/query_memory_pending.test`.

## 8. Q4: storage

> **Status: Q4a implemented** in all three cores. The cache rules in §8.6 record
> contracts the pager already keeps and add nothing to enforce.

Q1–Q3 charge memory that a **session** owns: a statement's buffers and state, and
its transaction's staged writes. Storage memory outlives every statement and is
shared by every session on the handle, so it cannot go on
`max_query_memory_bytes`. Charging committed pages to whichever session commits
would make one session's budget depend on what other sessions stored. Q4 therefore
uses **database-owned** accounts, and splits storage into two classes with opposite
contracts:

| Class | Owners | Bound | On exhaustion | Contract |
|---|---|---|---|---|
| **Committed storage** — non-refaultable bytes the database *is* | the `MemoryBlockStore` of an in-memory database and of each in-memory attachment | `max_storage_bytes` (new, §8.1) | the growing commit fails `54P06` | deterministic, cross-core (§8.4) |
| **Caches** — refaultable copies of durable pages | the file-backed leaf pool | `cache_bytes` (existing, [pager.md](pager.md) §3) | evict; **never fails** | none; residency is unobservable (§8.6) |

Session-local temp domains are neither. A session owns them, and `temp_buffers`
already bounds them with the same page basis ([temp-tables.md](temp-tables.md) §7),
so Q4 does not count them again.

The hazard Q4a closes: an untrusted session on an in-memory database can commit
many small, cheap transactions. Each one fits `max_cost` and Q3's per-transaction
bound, yet together they grow the database's RAM without limit. Q4a bounds what
the session can make the *database* hold.

### 8.1 The setting

`max_storage_bytes` is a **database setting**, not a session setting. It is fixed
when an in-memory database is created (`create(opts)` with no `path`, Rust
`CreateOptions { max_storage_bytes }`, Go `CreateOptions.MaxStorageBytes`, TS
`maxStorageBytes`) or when an in-memory database is attached (an option on the in-memory attach source,
Rust `AttachSource::memory().max_storage_bytes(n)`,
[attached-databases.md](attached-databases.md) §4). It can be changed on an open handle with
`set_max_storage_bytes` / `SetMaxStorageBytes` / `setMaxStorageBytes`, which takes
the target database name (`main` or an attachment) for attachments. Every session
on the handle shares it. It is not persisted, not transactional, and not reachable
from SQL. A positive value is the limit in bytes; **zero or negative means
unlimited, which is the default**, for the same reason Q1 has no finite default
(§2): a finite default would reject RAM-sized workloads that run today. This
retires the provisional `memory_limit` `CreateOptions` knob in TODO.md and gives it
the `max_*_bytes` name its siblings use.

Q4a covers **in-memory backings only**. Setting the limit on a file-backed database
or file attachment is `0A000`. The file form of the limit (a database-size cap, the
analog of SQLite's `max_page_count`) is a different measure, and §8.7 explains why
it stays deferred rather than reusing this one.

### 8.2 Measure

```
storage_bytes(domain) = page_count × page_size
```

`page_count` is the domain's **logical high-water mark**: meta slots, live pages,
and pages on the free list, everything the byte store holds. The free pages count
because an in-memory store keeps them allocated for reuse. This is the
`temp_buffers` basis, taken over unchanged
([temp-tables.md](temp-tables.md) §7), and it is honest in the same way: it charges
interior nodes, page headers, and space left sparse after deletes, which a
record-byte sum would miss. It is **not** the physical buffer length, which also
includes geometric preallocation slack that is outside the byte contract
([pager.md](pager.md) §7).

Within-session compaction ([temp-tables.md](temp-tables.md) §6; `maybe_compact`)
keeps `page_count` between `live` and roughly `2 × live`. So a limit of B bytes
reliably holds about B/2 bytes of live pages and may hold up to B. Hosts size the
limit with this in mind, and the public docs must say so.

### 8.3 Admission

> **Status: implemented (Q4a).** The repair exemption, the watermark rule, and the
> multi-root precheck below were refined while building it.

The check runs **once per commit** of an in-memory domain, after the commit has
planned its page allocation (dirty pages packed, ids assigned from the free list
first, then the high-water mark) and **before any page is written to the store or
the root is published**. The predicted post-commit `page_count` gives the exact
new `storage_bytes`, with no one-commit lag. A plan is admitted when any of these
holds, tried in this order:

1. **It fits:** the domain is unlimited, or the plan does not raise the high-water
   (`page_count(after) ≤ page_count(before)`, so a limit lowered below the current
   size still admits commits that fit in free pages), or
   `page_count(after) × page_size ≤ max_storage_bytes`.
2. **It fits after a forced compaction.** Periodic compaction can leave a domain
   at its limit with dead pages not yet on the free list. So the commit runs one
   forced compaction of the **committed** snapshot — the same reachability walk,
   from the last commit's catalog root plus the pages it wrote (which cover a GiST
   R-tree), without the periodic trigger — and, when that freed anything new,
   discards the plan's page assignment and re-plans against the larger free list.
   Compacting the committed version is safe when **no live reader pins an older
   version**: a reader *at* that version keeps every page the walk keeps. If an
   older version is pinned (a read session or streaming cursor that predates the
   last commit), there is no forced compaction; closing the old readers is the
   remedy. The walk is O(pages), but it only runs on a commit that would otherwise
   fail.
3. **The repair exemption:** the commit stages no record version (§7.1 — pure
   `DELETE`s and drops; `UPDATE`, upserts, and FK actions that write rows all
   stage) and rewrites no more catalog pages than the last commit did. Copy-on-write
   needs fresh pages for a delete's new path *before* the old path is dead, so
   without this rule a database at its limit could never delete its way back. The
   catalog clause keeps empty-object DDL (`CREATE TABLE` with no rows) from growing
   the database through the exemption. Such a commit can leave the domain over its
   limit by the pages it rewrote; the next commit's forced compaction reclaims the
   old path. That is bounded guardrail overshoot.

Otherwise the commit fails.

**Failure.** `54P06 storage_limit_exceeded`, "storage of database \"main\" exceeded
the limit of N bytes", naming the domain. Because nothing has been written yet, the
transaction is discarded like a serialization-only commit error, and the handle
stays usable without poisoning ([validated-cow.md](validated-cow.md)). An autocommit
statement commits nothing. A `COMMIT` of an explicit block fails and the
transaction ends rolled back, matching PostgreSQL, where a failed `COMMIT` does not
leave the transaction open. A forced compaction that ran before the rejection
stays in effect; it only changed which dead pages the free list holds.

**Multi-root commits.** A transaction that also dirties session temp tables or
in-memory attachments packs those domains before main persists
([attached-databases.md](attached-databases.md) §5), and packing a domain runs its
post-commit compaction. So when any such domain is dirty, **every limited in-memory
domain is prechecked before any domain packs a page**: attachments by name, then
main, each planned (with any forced compaction), checked, and its plan's page
assignment discarded; the real commit then re-plans the same allocation. A
rejection in any domain discards the whole transaction like a `ROLLBACK`, temp
changes and session sequence state included, and leaves every other domain's store
untouched.

**Ordering with the other gates.** Cost, `54P05`, and the Q3 end-of-statement check
(§7.3) all run *during or at the end of* the statement, before the commit starts.
So an autocommit write that breaks both budgets reports the earlier of them,
`54P01`/`54P02`/`54P05`, and the commit never runs. `54P06` is only ever reported
by a statement that has otherwise succeeded.

### 8.4 Determinism

`54P06` sits inside the contract, like `54P03` and unlike cache residency. Its trip
point is a pure function of the sequence of operations, because each input is:

- **Page allocation.** The copy-on-write commit serializer is deterministic, and
  so is the order it reuses free-list pages in. In-memory stores already share the
  file serializer.
- **The compaction schedule.** The `16`-page floor and the `2 × live` trigger
  are shared data: `[reclamation]` in `spec/cost/schedule.toml`
  (`compact_min_pages`, `compact_growth`), codegen'd into every core like the cost
  schedule. They were hand-mirrored constants before Q4a, though `54P03` already
  depended on them.
- **The reader watermark.** Which versions are pinned depends on the order of
  commits and pin points, and that order is the deterministic schedule the
  concurrency corpus already uses (CLAUDE.md §7). An in-memory database cannot be
  shared across processes, so no timing-dependent co-residence state enters the
  measure (§8.7).

The limit itself never changes results or cost; it only decides whether a commit
is admitted.

### 8.5 Interaction with Q3 and the scalar allowance

The three gates do not overlap. Q3 bounds what one transaction stages before
commit, per session. Q4 bounds what the database keeps after commit, per database.
The scalar allowance bounds churn within a statement. When a transaction commits,
its staged bytes leave Q3 and its pages arrive in Q4. For an untrusted surface over
an in-memory database, the host sets `max_query_memory_bytes` (per-session work and
staging) and `max_storage_bytes` (what everyone together may store). Neither one
substitutes for the other.

### 8.6 Caches: evict, never fail

Cache residency is unobservable by contract ([pager.md](pager.md) §3/§5): two cores
with different resident sets return identical results and cost. A cache account
therefore **may never produce an error**. If an evictable page could abort a
query, residency, and with it eviction timing, thread scheduling, and CLOCK's hand
position, would become part of the observable surface. So a cache's only response
to pressure is eviction, and `cache_bytes` remains its account. Q4 adds no new
check here. It records what `cache_bytes` does and does not bound, so the docs can
state it accurately:

- **Bounded:** the clean leaf pages in the file-backed pool, at
  `max(1, cache_bytes / page_size)` leaves.
- **Overshoot by live references:** an evicted leaf stays alive while a traversal
  or cursor still references it. This is at most one root→leaf path per active
  traversal plus each open streaming cursor's current leaf: concurrent readers ×
  tree height. It is guardrail overshoot.
- **Not bounded by `cache_bytes`:** the **interior skeleton**, always resident and
  roughly `page_count / fan-out` pages ([pager.md](pager.md) §1); the **resident
  GiST R-tree**, eager on both backings and proportional to indexed rows
  ([gist.md](gist.md) §4); and in-memory databases' pinned pool, which caches the
  committed storage of §8.1 and is accounted there. For in-memory domains,
  `max_storage_bytes` bounds the skeleton and the GiST tree indirectly, because
  both are proportional to page counts it limits. For file-backed databases they
  are bounded by file size until interior paging and GiST demand paging land
  (pager.md §6, gist.md §11).

The handle gains one read-only **deterministic gauge**, `storage_bytes(name)`
(Rust/Go/TS idiomatic spellings, like `resident_leaves`). It reports §8.2's measure
for in-memory domains and `page_count × page_size` for file-backed ones. In both
cases it is the logical committed high-water mark, not RSS, so a host can watch
growth against its limit before it is hit.

### 8.7 Why the file form is deferred

For a file-backed database, `page_count × page_size` is not deterministic under
shared multi-process access. While another process is co-resident, commits are
append-only (`free_list_head = 0`, [locking.md](locking.md)). Co-residence is
detected by a background probe, so how much the file grows depends on timing that
no single handle's operation sequence fixes. A file-size cap therefore needs a
different measure: the **live** page count (reachable pages), which is a function
of the committed trees alone and does not depend on reuse, watermark, or
co-residence. It also needs an exact incremental orphan count per commit. The GiST
whole-tree rewrite makes that count non-trivial today. The cap answers a different
need (disk use, analogous to `53100 disk_full`) from Q4's (RAM), so it is a
separate follow-on and not foreclosed. Rejecting the setting on a file database
with `0A000` keeps that door open: a later file form can adopt the same option
name with a stated measure, instead of silently reinterpreting an accepted value.

### 8.8 Not covered

Q4 leaves these outside every account: the interior skeleton and GiST R-tree of
file-backed databases (§8.6), the catalog (proportional to schema size; DDL is
gated by `allow_ddl`), persisted statistics (bounded by the statistics target,
[statistics.md](statistics.md)), host-owned prepared statements and their plan
caches, and per-core representation overhead beyond the logical page bytes. A
core may hold a faulted in-memory leaf as a copy of its store block, which is
bounded overshoot of at most one extra page per resident leaf.

### 8.9 Slices

- **Q4a — committed in-memory storage.** The setting (§8.1), measure, admission,
  and forced compaction (§8.2–§8.3), the `54P06` registry entry, the promoted
  compaction constants (§8.4), and the `storage_bytes` gauge (§8.6), in all three
  cores. Corpus: a database-level `# max_storage_bytes: N` directive (applied at
  `create`, in-memory mode only; capability `resource.storage_memory`) and
  `resource/storage_memory.test`, which pins exact trip points, the
  shrink-and-repair exemption, and `DELETE` recovery through forced compaction. A
  concurrency-format entry pins the watermark case: a pinned reader blocks
  reclamation, so a commit is rejected `54P06` and succeeds once the reader closes.
  Per-core tests cover the host API: setting on `create`/`attach`, the runtime
  setter, `0A000` on file backings, and multi-root rejection publishing nothing.
  `rake conformance:query_memory` also records each record's final in-memory
  `storage_bytes` and fails unless all cores agree. That cross-core check is what
  backs the §8.4 claim, the same way peak balances back Q1–Q3.
- **Q4b — documentation only.** Public docs ([resource-limits](../../web/src/routes/docs/api/resource-limits/+page.md))
  describe the cache rules of §8.6 and the B/2–B sizing note of §8.2. No engine
  change.
- **Follow-ons:** the file-backed size cap on live pages (§8.7); bounding the
  interior skeleton and GiST tree under `cache_bytes` once they page.

## 9. Rollout gates

Each slice lands in all three cores together with: corpus entries pinning exact
`54P05` thresholds for its owners (`# max_query_memory_bytes: N`, capability
`resource.query_memory`), shared size vectors for any new measurement, and
per-core tests for host-API collectors and cursor lifetimes. Documentation states
which owners are covered; `max_query_memory_bytes` must not be described as a
process or heap limit.

`rake conformance:query_memory` runs the whole corpus with accounting active in
both storage modes, and once more on disk with `work_mem` forced low so every
spill-capable structure spills. Each core records every record's **peak balance**
(its minimal passing budget) and the task fails unless all cores agree on every
record — the whole corpus, not only the records that pin a threshold, is the
cross-core check of the reserve and release sites.
