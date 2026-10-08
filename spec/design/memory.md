# Memory admission

Cost is a deterministic work allowance, not a memory limit. A large value can
allocate substantial memory before another row or expression is visited, and a
join can copy one large value into many rows for a handful of cost units. Memory
admission therefore runs **independently of cost**, through two per-statement
accounts:

| Account | Setting | Default | Model | Error |
|---|---|---|---|---|
| Scalar allocation | `max_scalar_bytes` | 64 MiB | cumulative churn, never refunded | `54P04` |
| Query memory | `max_query_memory_bytes` | unlimited | live logical bytes, released when dropped | `54P05` |

Both are **guardrails, not heap caps.** They bound logical bytes computed from a
shared, representation-independent schedule, so an abort is deterministic and
cross-core identical ([determinism.md](determinism.md) §3: a limit hit is part of
G1/G2, not a ledger exception). Neither equals process RSS, and both allow bounded
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
release never errors and never takes `used` below zero.

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
| Q3 | **Pending writes** — a transaction-owned account for staged inserts/updates/deletes, surviving statement boundaries and released at commit/rollback | planned |
| Q4 | **Storage** — database-owned accounts for page caches and committed in-memory storage | planned |

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
results. Operator state is Q2 (§6); a single base relation's storage read before
its rows are admitted and pending writes remain outside the account.

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

## 7. Rollout gates

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
