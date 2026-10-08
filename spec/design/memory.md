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
point, so enabling a budget that is never exceeded cannot change whether, or
where, a cost ceiling aborts.
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
| Q2 | **Operator state** — hash-join tables, group/distinct/dedup sets and keys, aggregate accumulators (`json_agg`/`jsonb_agg` and the other JSON aggregates, ordered-set and hypothetical buffers), sort buffers and top-k heaps, window partition/frame state, spill spools' resident buffers, recursive-CTE dedup sets | planned |
| Q3 | **Pending writes** — a transaction-owned account for staged inserts/updates/deletes, surviving statement boundaries and released at commit/rollback | planned |
| Q4 | **Storage** — database-owned accounts for page caches and committed in-memory storage | planned |

Q2 operators that can spill must charge only their resident portion and release it
as they spill; an operator without spill support (or a host with no scratch
target) fails at the gate instead. A single value too large to admit fails even
when its containing operator can spill. Until Q2 lands, `work_mem` remains the
only bound on spill-capable operator state, and in-memory databases (which never
spill) leave operator state unbounded by this account.

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
- The **columnar and vectorized lanes** gather dense column lanes that are
  operator state (Q2); they charge their group rows and collected output.
- The **streaming external sort** lane's input (the sorter's buffer or the
  collated survivor buffer) is sort state (Q2).
- The **bounded-spill lane** (file-backed aggregation, DISTINCT, and multi-way or
  hash joins) holds its rows in spools (Q2); its window stage materializes its
  rows as a Q1 buffer while the window runs.

Q1 therefore bounds row-buffer amplification — cross joins, set-returning
functions, recursive CTE output, wide projections of large values, and
materialized results — but not operator state, a single base relation's storage
read before its rows are admitted, or pending writes.

## 6. Rollout gates

Each slice lands in all three cores together with: corpus entries pinning exact
`54P05` thresholds for its owners (`# max_query_memory_bytes: N`, capability
`resource.query_memory`), shared size vectors for any new measurement, and
per-core tests for host-API collectors and cursor lifetimes. Documentation states
which owners are covered; `max_query_memory_bytes` must not be described as a
process or heap limit.

`rake conformance:query_memory` runs the whole corpus with accounting active in
both storage modes. Each core records every record's **peak balance** (its minimal
passing budget) and the task fails unless all cores agree on every record — the
whole corpus, not only the records that pin a threshold, is the cross-core check
of the reserve and release sites.
