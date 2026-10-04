# Memory admission

Cost is a deterministic work allowance, not a memory limit. A large value can
allocate substantial memory before another row or expression is visited. Memory
admission therefore runs **before allocation**, independently of cost.

## Implemented: scalar allocation allowance

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

This allowance **does not cover all scalar kernels or all query memory**. In
particular, existing inputs, concatenation/replacement, arrays/JSON, decoded rows,
join/aggregate/distinct tables, result buffers, and pending writes are not covered
by this first reservation surface. It must not be advertised as a whole-query
memory cap. `work_mem` controls sort, hash JOIN, aggregate, and DISTINCT spilling;
their row/state spools preserve evaluation order and costs ([spill.md](spill.md)).
Upstream materialized producers, window partitions, final scalar values, and
host/result collectors remain separate allocation owners. `temp_buffers` remains
the session-local committed temp-storage cap. Neither is a whole-query heap cap.

## Decided contract: whole-query memory (not yet implemented)

The next gate is `max_query_memory_bytes`, a finite host-configured live logical
memory budget with default 256 MiB, separate from cost, scalar allocation churn,
`work_mem`, the page pool, and temp storage. It must account for every concurrent
allocation owner inside a query, rather than granting each operator the full
budget. It may be raised by the host; SQL cannot weaken it. No core may advertise
this setting until the admission surface is complete in all three cores.

The accounting contract is a shared representation-independent schedule:
32 bytes per owned value node, 16 bytes per container slot, UTF-8 payload bytes
for text, raw bytes for bytea, 4 bytes per logical base-10^4 decimal group,
recursive child charges for arrays/composites/ranges/JSON, and explicit byte
charges for encoded keys, hash buckets, sort metadata and spill buffers. Before
implementation these facts belong in generated spec data and golden size vectors;
allocator overhead is not inferred from Rust/Go/JS object sizes.

Every owner obtains a reservation before constructing or growing its allocation.
Moves transfer the reservation, copies reserve again, and release returns capacity.
A streaming cursor retains reservations for its internal live buffers until close;
values handed to the host transfer out of query ownership. Materializing APIs must
admit their accumulating result buffers before appending, so draining a cursor
cannot bypass the limit through an engine-owned collector. Host-retained results
and host functions are the host's responsibility.

Blocking sort, hash join, GROUP BY, DISTINCT, window partitions, CTE buffers and
set operations all draw from the same statement budget. Spill-capable operators
release admitted buffers as they spill; an operator without spill support fails
before allocation. Hosts without a scratch device fail closed at the same gate.
A single value too large to admit fails even when the containing operator can spill.

Pending writes need a **transaction-owned** reservation account that survives
statement boundaries and releases on commit/rollback; otherwise many small INSERTs
inside BEGIN evade a per-statement limit. Pager caches and committed in-memory
storage need separate database-owned accounts. Combined budgets and host process
limits, rather than a claim that logical bytes equal RSS, bound deployment memory.

Rollout gates: shared size fixtures; complete scalar/codec admission; row/result
and CTE admission; blocking-operator reservations/spill; pending-write accounting;
then cross-core concurrency, cursor-lifetime and failure-cleanup tests. Until these
land, documentation states the implemented narrower guarantees explicitly.
