# Streaming the result cursor — design

> The reasoning behind making the `Rows` cursor a **pull source** — yielding each row as the
> caller asks for it, instead of running the whole query and handing back a fully materialized
> result. This is a *design* doc; the API shape is [api.md](api.md) §4/§9, the cost contract is
> [cost.md](cost.md), the operator-memory bound is [spill.md](spill.md), the demand-paged storage
> is [pager.md](pager.md), the lazy large-value read is [large-values.md](large-values.md) §14,
> and the snapshot/lifetime model is [transactions.md](transactions.md) §5/§8. When a decision
> here changes, update [CLAUDE.md](../../CLAUDE.md) §9 and [api.md](api.md) §4/§9 in the same edit.

This is the **"true streaming iterator"** item: the further step api.md §4/§9 names a non-goal in
the prepared-statement slice. The cursor contract was *designed* streaming-ready from the start —
"yield row, then row, then column metadata" is exactly what a pull executor satisfies (api.md §4)
— so this lands **without changing any caller**, only what sits behind the cursor.

## 1. The gap this closes (and the three already closed)

A database serves a result one of two ways: **materialize** (run the whole query, build the full
result, then let the caller walk it) or **stream** (yield each row as the caller pulls, doing the
work incrementally). PostgreSQL and SQLite both stream — and both lazily decode at every level
below the row (§2). jed materializes the result, but has **already** closed the lower lazy-decode
gaps:

| Level | jed status | Where |
|---|---|---|
| **Pages** | ✅ lazy (demand-paged) | bounded CLOCK buffer pool ([pager.md](pager.md), P6.4); leaves faulted on access, only the interior skeleton resident. |
| **Large / spilled values** | ✅ lazy (per touched column) | `Unfetched` references resolved only for the statically-touched columns ([large-values.md §14](large-values.md), `resolve_columns`/`resolveColumns`). An unread large column reads zero overflow pages. |
| **Small inline columns** | ❌ eager | a leaf decode (`decode_leaf_node`) materializes every inline value of a record into the `Row`; only *large* untouched values stay `Unfetched`. The one remaining lazy-decode gap vs. PG/SQLite — a separate, cost-neutral follow-on (§8). |
| **The result rows** | ❌ eager — **this doc** | `exec_select_plan` returns `SelectResult { rows: Vec<Vec<Value>> }`; `Rows` is an iterator over that detached, fully-built slice. `query()` runs the whole query *before* the caller sees a row. |

The lazy-decode foundations (pages, large values) landed as **independent** slices, decoupled from
streaming by the *static touched-set* cost contract ([cost.md](cost.md) "The touched set"). They do
**not** need to be redone here. Streaming is the orthogonal remaining piece; it *benefits* from the
existing laziness (a cursor that stops early never faults the leaves or decompresses the values it
never reaches) but does not depend on extending it.

### What "streams" today is the *input*, not the *output*

The executor already has four input-streaming paths — the LIMIT short-circuit, ORDER-BY-satisfied-
by-PK-scan, ORDER-BY-satisfied-by-secondary-index, and the external-merge-sort streaming feed
([cost.md](cost.md) "LIMIT short-circuit" / "ORDER BY satisfied by …", [spill.md §5](spill.md)).
Each avoids materializing the scan *input* — but every one still builds the full **output** `Vec`
before returning it to the cursor. The `Sorter` / `SortedRows` (`SortedRows::next()`) is the one
genuine pull abstraction already in the tree, and it is the model the blocking operators generalize
to (§4).

## 2. What PostgreSQL and SQLite do (the reference behavior)

Both are pull/demand-driven executors that lazily decode at every level — confirming the target
shape and confirming that jed's already-landed laziness is the right foundation.

- **PostgreSQL** — a **Volcano/iterator** executor: `ExecProcNode` pulls one tuple at a time up the
  plan tree; portals and `FETCH` (and the extended-query row-limit) stream to the client. Lazy
  column decode: `slot_getsomeattrs(n)` deforms a tuple only up to the **highest referenced
  attribute** — trailing columns are never deformed. Lazy large values: a TOAST pointer is
  **detoasted on demand** (`pg_detoast_datum`) only when a function needs the full datum.
  Demand-paged via `shared_buffers` (clock-sweep). Blocking nodes (Sort, HashAggregate, Hash,
  Materialize) buffer; everything between them streams.
- **SQLite** — the **VDBE**, a register bytecode VM: `sqlite3_step()` yields **one row per call**
  (`OP_ResultRow`) — true pull, the result is never materialized. Lazy column decode: `OP_Column`
  parses the record header and extracts **one column on demand**, caching offsets and stopping at
  the max referenced column. Lazy large values: overflow-page chains read on demand. Demand-paged
  via the pager's page cache. Blocking: `ORDER BY`/`GROUP BY`/`DISTINCT` without a usable index use
  `vdbesort.c` — **external merge sort with spill** (jed's [spill.md](spill.md) is the same
  lineage); otherwise it streams.

**Net:** both = pull executor + lazy column decode + lazy large-value + demand-paged pages +
materializing blocking operators. jed already matches them on pages, large values, and spilling
sort. The two differences are exactly the two ❌ rows in §1: no pull cursor (this doc) and eager
small-inline-column decode (§8).

## 3. Scope: a top-level pull cursor (not a volcano rewrite, not a VDBE)

The decided scope is the **top-level pull cursor**: make the executor return a `Cursor` the `Rows`
cursor drives, where the **non-blocking single-table pipeline streams lazily** and the **blocking
operators buffer internally and yield their buffer lazily**. This is deliberately *not* a full
Volcano operator-tree rewrite, and *not* a bytecode VM. The rationale:

- **The blocking operators must buffer anyway.** A `Sort`/`DISTINCT`/aggregate/`JOIN` cannot emit
  its first output row until it has seen (much of) its input — so a full Volcano `next()` on those
  nodes buys nothing the buffered-then-streamed form does not, at far higher refactor risk across
  three cores. The win — bounded peak *output* memory, first-row latency, and early termination —
  comes from the non-blocking pipeline and from yielding the blocking buffer lazily, both of which
  the top-level cursor delivers.
- **It is the lowest-risk cross-core change.** Each core lands it cost/result-neutral under full
  drain (§6), so each lands green independently (the P5.1 / P6.4b precedent).

### Relationship to a future bytecode VM

A bytecode VM (SQLite's VDBE) and a Volcano tree are both pull/demand-driven — **a VDBE is just the
*compiled* form** (the operator tree linearized into an opcode loop with a program counter). No
scope choice here forecloses one, and the chosen scope is a **step toward** one:

- **The pull B-tree scan cursor (§5) is the single biggest VDBE prerequisite, and it is built
  here.** A VDBE's `OP_Rewind`/`OP_Next`/`OP_Column` map directly onto a stateful pull cursor; a
  **push-based callback scan cannot drive a VDBE** (the VM owns the control flow), so jed's current
  `scan_range(visit)` would have to be replaced regardless. Building the pull cursor now removes the
  one thing that would actively block a VDDE later.
- The other VDBE needs — a statement that **pins a read snapshot across `step`s** (the lifetime
  model §5, which a VDBE prepared statement also has), **cost accrued as you step** (§6), and the
  **cursor-as-pull-source API** (`Rows.next()` ↔ `sqlite3_step`) — are all built here too.
- A **full Volcano model** is also VDBE-compatible (a clean `next()` algebra is a good IR to lower
  from), but it is not a VDBE *prerequisite* (SQLite reached its VDBE with no Volcano interpreter
  in between) and its virtual-call generality is what a VDBE optimizes away — so it is a larger lift
  that does not *uniquely* enable a VDBE.

**The real jed-specific question a VDBE raises is not foreclosed by this work — it is *constrained*
helpfully by it.** jed has three hand-written cores in lockstep, *no reference implementation* (§2),
and a **byte-identical cost contract** accrued at fixed sites in a fixed order ([cost.md](cost.md)
§3). A VDBE in one core would have to accrue the identical units in the identical order as the
tree-walkers in the others (or all three cores would need identical bytecode — a heavy lockstep
burden that arguably smuggles a reference implementation in through a shared opcode spec). That is a
far bigger decision than streaming. Streaming, done right, **writes down the accrual-order contract
a future VDBE must honor** (§6, the mirrored-loop rule) rather than narrowing the options — so this
work is groundwork a VDBE would build on, never against.

## 4. Architecture: the `Cursor` a `Rows` drives

`exec_select_plan` stops returning a `SelectResult { rows }` and returns a **`Cursor`** — a pull
source with one method, conceptually:

```
Cursor.next() -> Option<Vec<Value>>     # the next projected output row, or None at end
Cursor.column_names() -> &[String]
Cursor.cost() -> i64                     # accrued so far (final only after the cursor is drained — §6)
Cursor.close()                           # release the pinned snapshot (§5); idempotent
```

`Rows` becomes a thin wrapper that delegates `next()`/`column_names()`/`cost()`/`close()` to its
`Cursor`. The cursor comes in two shapes, chosen by the plan:

- **`Streaming`** — the non-blocking single-table pipeline (single table, no join, aggregate,
  `DISTINCT`, or window). It holds a **pull B-tree scan cursor** (§5) and runs scan → resolve
  touched columns → `WHERE` → project, **one row per `next()`**. This subsumes the existing
  LIMIT-short-circuit, ORDER-BY-by-PK, and ORDER-BY-by-index paths ([cost.md](cost.md)): they were
  already row-at-a-time internally and were materializing only because the cursor demanded a `Vec`.
  Peak memory is one row (plus the scan's root→leaf path of pinned nodes). A caller that stops early
  faults no further leaves and produces no further rows.
- **`Buffered`** — every plan with a blocking operator (`ORDER BY` the scan doesn't satisfy,
  `DISTINCT`, aggregate/`GROUP BY`, window, multi-table join, set operation, `VALUES`, a
  materialized CTE). On first `next()` it runs the blocking part to completion into its existing
  intermediate (the sort buffer / group rows / distinct set / join output / set-op result — itself
  already bounded by [spill.md](spill.md) for `ORDER BY`), then yields from that buffer **one row
  per `next()`**. This is the `SortedRows::next()` pattern generalized to every blocking operator:
  the input is buffered (correctly — it must be), but the **output is streamed**, so the caller and
  any enclosing operator never hold the full result, and a `LIMIT` over the buffer stops pulling
  once the window is filled.

The split is exactly the PG/SQLite split (§2): pipeline between blocking points, buffer at them.

**Per-core realization** (not a byte contract — like the pager and the spill machinery, each core
implements it idiomatically; the only contract is results + cost, §6):

- **Rust** — `Cursor` is an enum (`Streaming` / `Buffered`) implementing `Iterator<Item =
  Vec<Value>>`; the scan cursor is a struct holding a `Vec` of `(node, index)` frames over the
  persistent map; `close` is `Drop`.
- **Go** — `Cursor` is an interface (or a struct with a `kind` tag); the scan cursor is a stateful
  struct with an explicit frame stack; `Close()` is **explicit** (no destructor).
- **TS** — `Cursor` wraps a **`function*` generator** (`yield`-per-row makes the pull pipeline
  natural — the one core where streaming is structurally easy); `close()` is **explicit** (and
  `return()`s the generator). The existing `scanSource` generator (today only a cost-metering wrap
  over a materialized array) becomes a real lazy source.

## 5. Snapshot lifetime — PG-faithful pinning (decided)

A streaming cursor reads from live storage as it is pulled, so it must **pin its read snapshot for
its whole life** — the PG portal / SQLite prepared-statement model, the decided option.
[transactions.md §5](transactions.md) already promises "a `Rows` cursor is snapshot-stable for its
life"; streaming makes that promise *load-bearing* rather than a free consequence of materializing.

- **What is pinned.** The cursor pins the **root it reads, as of `query()` time** — the committed
  snapshot for an autocommit one-shot or a `READ ONLY` transaction; the transaction's `working`
  root for a cursor opened inside an explicit `READ WRITE` block. The persistent map is
  copy-on-write — a published or working node is **never mutated in place** ([transactions.md
  §2](transactions.md)) — so the pinned root stays valid and stable for the cursor's life even as a
  writer advances to new roots. (A buffer-pool node the cursor's path holds is kept alive by the
  live reference / `Arc` / pin even if evicted from the cache — [pager.md §4](pager.md).)
- **Registration in the watermark.** A streaming cursor is a **live reader**: it registers its
  pinned version in the `Database`'s live-reader registry ([transactions.md §8](transactions.md)),
  and `close()` / drain-to-exhaustion deregisters it, advancing `oldest_live_txid`. This is the
  exact machinery the concurrent-reader sessions already use; a streaming cursor is just an
  additional pin on it. (Today's reclamation is reconstruct-on-open only, so a long-lived cursor is
  already safe trivially; registration is what keeps the *continuous-reclamation* follow-on
  ([transactions.md §8](transactions.md)) safe without a retrofit.)
- **Close obligation.** Rust releases on `Drop`; **Go and TS need an explicit `close()`** (no
  destructor). The ergonomic iterators already close on loop exit — Go `All()`/`Collect`, TS
  `*iterate` (api.md §11.3) — so they are covered; the raw `Rows` cursor gains a documented `close`
  obligation, and draining it to exhaustion also releases. A forgotten-open cursor pins an old
  snapshot and **delays free-list reclamation** — the same bounded risk PG carries with a held
  cursor and SQLite with an unfinalized statement, mediated by the existing watermark.
- **Autocommit interaction.** Under autocommit, `query()`'s implicit read transaction **stays open
  until the cursor is drained or closed** (the cursor *is* the transaction's lifetime). This is the
  one observable lifecycle change from the materialized era, and it is PG-faithful (a portal holds
  its transaction). Consecutive autocommit reads still each get their own snapshot ([transactions.md
  §5](transactions.md)); a held streaming cursor simply keeps reading the snapshot it opened on.
- **Read-your-writes is preserved; one uniform cursor-visibility rule.** Freezing the *open cursor*
  at its open-time root does not weaken read-your-writes for the *transaction*: later statements in
  an explicit block read the latest `working` root and so see every write the transaction has made —
  including writes after the cursor opened — while only the in-flight cursor is frozen. jed offers a
  **single** cursor-visibility rule — every cursor is **insensitive and forward-only, frozen at its
  open-time root** — which **matches PostgreSQL's default (non-`WITH HOLD`) cursor** for the
  in-transaction case (a PG cursor likewise freezes its snapshot at open, sees none of the
  transaction's later writes, while later non-cursor statements do). The simplifications versus PG
  are scoped and named: **no `WITH HOLD`** (a cursor opened in an explicit block is bound to it — see
  *End-of-block lifecycle* below), **no sensitivity / scroll / `WHERE CURRENT OF` knobs** (the one
  insensitive, forward-only behavior), and **the one real divergence** — an **autocommit** cursor
  holds its snapshot across subsequent autocommit statements (SQLite-style), where strict PG
  autocommit would bind a cursor to a single statement (the *Autocommit interaction* bullet above).
  All three are simplifications of, not contradictions with, the per-case PG answer; the canonical
  in-transaction case agrees with PG outright.
- **End-of-block lifecycle.** Committing or rolling back the explicit block that opened a cursor
  **closes any cursor still open** — its unread rows are discarded and its snapshot pin released —
  matching PostgreSQL's close of non-`WITH HOLD` cursors at transaction end. This is the inverse of
  [api.md §2.3](api.md)'s "`close()` rolls back an open explicit transaction": there a cursor's
  `close` can end the transaction; here the transaction's end closes the cursor. Because jed has no
  `WITH HOLD`, no cursor outlives its block. (Under autocommit there is no enclosing block, so the
  cursor lives until drained or `close`d — the *Autocommit interaction* bullet.)
- **In-memory vs file-backed.** Identical model both ways; an in-memory database has no free-list to
  gate, so the pin is bookkeeping only. The single-writer gate is untouched — a streaming read
  cursor never holds the write gate.

## 6. Determinism & cost — invariant under full drain (the contract)

This is the load-bearing simplification, in the same spirit as the buffer pool ([pager.md
§3](pager.md)) and the spill sorter ([spill.md §6](spill.md)): **streaming changes *when* work
happens and *when* cost accrues, never *what* a fully-drained query observes.**

- **Cost accrues during `next()`, not during `query()`.** The same units fire at the same sites for
  the same rows; they are merely pulled forward to when the caller asks for each row. Under **full
  drain** (every row pulled) the accrued **total is byte-identical** to the materialized path — so
  every `# cost:` corpus value holds unchanged.
- **The conformance harness drains fully.** The corpus contract is a query's complete result
  multiset + total cost (compared `rowsort` where unordered — CLAUDE.md §8). The per-core harness
  therefore **drains the cursor to exhaustion, reads `cost()` after drain, and surfaces any error
  raised mid-drain as the statement's error** (a `54P01` cost abort or a runtime error may now
  surface during iteration rather than at `query()`). With that harness contract — a small per-core
  change where not already true — streaming is **corpus-transparent**: same rows, same total cost,
  same errors. So streaming adds **no new corpus capability flag**; it is internal machinery whose
  only contract is results + cost, exactly like the buffer pool, the spill sorter, and P5.3
  concurrency.
- **The mirrored-loop accrual-order rule.** A streaming non-blocking pipeline **interleaves**
  accrual (scan row, then produce row, then next scan) where the eager path **phased** it (scan all,
  then produce all). The total and the result are identical; only the *instant* accrued cost would
  cross a `max_cost` ceiling shifts — and a ceiling fires at exactly `accrued == limit` either way
  ([cost.md §6](cost.md)), so *whether* a statement aborts is invariant (it depends on the total,
  which is invariant). [spill.md §6](spill.md) already set this precedent for the fused scan→sort
  feed; the binding requirement is that **all three cores mirror the streaming loop structure** so
  the accrual order — and thus the deterministic abort point — is identical cross-core. No corpus or
  per-core test pins the intermediate abort *position*; the contract is the total and the
  whether-it-aborts.
- **`cost()` is final only after drain.** While a cursor is mid-iteration, `cost()` reflects rows
  pulled so far. Documented on the cursor (api.md §4); the harness reads it post-drain.
- **Early termination is outside the corpus, like cancellation.** A caller that stops pulling before
  exhaustion does **less** work and accrues **less** cost — a genuine, beneficial, PG/SQLite-faithful
  behavior, but one that cannot be expressed as a SQL construct in the corpus (which always drains).
  So it is **per-core unit-tested only**, the exact treatment cancellation gets (api.md §11.4): it
  never yields *wrong* rows or a *different* total for a completed query, so the determinism contract
  — which is about queries that complete — is untouched. (The cursor's cancellation re-check in
  `next()`, already wired as the "forward-compatible hook for the streaming cursor" — api.md §11.4
  — becomes the site where mid-statement cancellation actually lands.)

### What does NOT change

- **The §8 byte contract.** The on-disk format, key encoding, and goldens are untouched — streaming
  is an executor/cursor change, no `format_version` bump.
- **Blocking operators still buffer their input** (correctly), and `ORDER BY` still spills under
  `work_mem` ([spill.md](spill.md)). Streaming bounds the *output*, not the blocking input — the
  spilling hash aggregate / `DISTINCT` / hash join remain the [spill.md §7](spill.md) follow-ons.
- **Lazy page + large-value decode** (§1) are already done and unchanged.

## 7. Slicing (the mergeable steps)

Sequenced **seam-first** so the risky control-flow change lands alone on a frozen seam, each step
independently testable and cost/result-neutral under full drain (so each core lands green
independently — the P6.4 precedent):

- **S0 — spec (this doc).** + revise [api.md](api.md) §4/§9 (streaming lands; the cursor pins a
  snapshot; `cost()` final-after-drain) and add the TODO.md entry. *No code.*
- **S1 — the `Cursor` seam (no observable change).** `exec_select_plan` returns a `Cursor`; its only
  shape is `Buffered`, wrapping today's materialized `Vec`. `Rows` delegates to the `Cursor`. Pure
  refactor — results, cost, goldens byte-unchanged (the P6.4a "seam first" move). The harness learns
  to drain + surface mid-drain errors (§6).
- **S2 — the pull B-tree scan cursor.** Convert the scan from push (`scan_range(visit)`) to a pull
  cursor (frame stack over the persistent map) in Rust/Go; a generator in TS. Internal; the existing
  push `scan_range` can stay for the mutation paths initially. The §3 VDBE-prerequisite.
- **S3 — stream the non-blocking pipeline + snapshot pinning.** ✅ **Landed (all three cores).** The
  `query()` → `Rows` path now serves the single-table no-blocking-operator read (the PK-ordered /
  LIMIT-short-circuit shape `streaming_scan_eligible` gates — shared with the eager `exec_streaming_scan`
  so the two never drift) through a lazy **`Streaming`** cursor: scan-cursor (S2) → resolve touched
  columns → `WHERE` → project, **one row per `next`**, accruing the identical cost units at the identical
  sites as the eager path. The cursor **owns a frozen snapshot** (Rust: a snapshot `Engine` built from
  the visible root + a copy of the session envelope, sharing the seam via `Rc` + the lifetime gauge;
  Go/TS: a captured snapshot engine sharing the seam by reference), so the returned `Rows` is
  self-contained and survives the transient `Database::query` session (§5). The §5 snapshot pin is
  registered in the live-reader watermark (`reader_pin` on the shared core; deregistered on cursor
  `close`/drop), and `close` releases it. **Scope this slice:** only the `query()`/`Rows` surface streams
  — the conformance corpus drives `execute()` → a materialized `Outcome` (untouched, so the corpus is
  green by construction); the index-order scan, streaming sort, and streaming join stay buffered (S4),
  and a write-classified statement (incl. a `nextval`/`setval` SELECT, `stmt_is_write`) never streams. A
  mid-drain error (a `54P01` cost abort, a `57014` cancellation, an arithmetic trap) surfaces during
  iteration (Rust stashes it for `Rows::error()`; Go sets `Rows.Err()`; TS throws out of the iterator).
  Verified per core by unit tests: `query()` == `execute()` rows + total cost under full drain, early
  exit charges less, the snapshot pin + watermark, and the mid-drain abort. Prepared-statement streaming
  and a `Database::query` watermark on the bare single-handle path are follow-ons (the bare path streams
  but pins nothing — the single-handle reclamation is reconstruct-on-open-safe, §5).
- **S4 — lazy output from the blocking operators.** ✅ **Landed (all three cores).** The `query()` →
  `Rows` path now serves a **blocking** read — a non-PK-ordered `ORDER BY`, `DISTINCT`, aggregate /
  `GROUP BY`, window, or a join — through a lazy **`Buffered`** cursor (`BufferedScan` in Rust,
  `bufferedScanCursor` in Go, a `bufferedRows` generator in TS), the generalization of the spilling
  sorter's pull iterator to every blocking shape. The seam is a new `exec_select_emit` extracted from
  `exec_select_plan`: it runs the **blocking part** (scan / join / `WHERE` / window / `ORDER BY` /
  `GROUP BY` / `DISTINCT`) and returns an **`Emitter`** — either a windowed **`Buffer`** (the
  intermediate rows, with a per-mode `Project` | `Identity` flag: `Project` evaluates the projection
  list on emission; `Identity` is the already-projected DISTINCT dedup output) or a **`Final`** result
  (the special input-streaming paths — `exec_streaming_scan`/`exec_index_order_scan`/
  `exec_streaming_sort`/`exec_streaming_join` — which already projected + charged). `exec_select_plan`
  **drives the same `Emitter` eagerly** (the materialized `execute()` path the corpus drives — rebuilt
  identically, byte-unchanged), and the lazy cursor **drives it row by row**: it owns a frozen snapshot
  engine (§5; sharing the seam + the lifetime gauge, like S3), runs the blocking part on its **first
  pull** (so a `54P01` cost abort / cancellation / arithmetic trap surfaces *during iteration*, not at
  `query()` — §6), then emits its buffer one row at a time. The win: **bounded peak *output* memory**
  (the output `Vec`/slice is never built on `query()`) and **top-N pulls over the buffer** — a caller
  that stops early skips the projection (`row_produced` + projection `operator_eval`s) of the rows it
  never pulls. Under **full drain** the rows + total cost are **byte-identical** to the eager path (the
  same `Emitter`, charged at the same sites in the same order — §6), so the corpus stays green by
  construction; verified per core by unit tests (`query()` == `execute()` rows + cost under full drain,
  buffered early-exit charges less, the snapshot pin + watermark, the mid-drain abort). The shared
  `stmt_rng` threads the per-statement entropy through the blocking part **and** the deferred
  projection, so a projection-list `uuidv7()`/`now()` draws the identical sequence whichever drive runs
  it. **Scope this slice:** the **top-level set-operation / `WITH`** read landed separately (S6, below);
  the **`exec_streaming_sort` output** went lazy via the `SortedRows` pull iterator separately (S7,
  below); and the other special input-streaming `SELECT` paths (`exec_streaming_scan` /
  `exec_index_order_scan` / `exec_streaming_join`) reach the lazy cursor as `Final` (eager-built on the
  first pull, then yielded lazily — no regression, since they are LIMIT-gated or already stream their
  input).
- **S5 — lazy small-inline-column decode — superseded.** Spun out and **promoted** to its own
  storage-core reshape in [lazy-record.md](lazy-record.md); see §8. *No longer a slice of this item.*
- **S6 — lazy DEFERRED set-operation / `WITH`.** ✅ **Landed (all three cores).** The `query()` → `Rows`
  path now serves a **top-level set operation** (`UNION`/`INTERSECT`/`EXCEPT`) or **pure-query `WITH`**
  through a lazy **deferred** cursor (`DeferredResult` in Rust, `deferredCursor` in Go, an inline
  `RowSource` in TS), wired after the `Buffered` lane (`try_deferred_query` / `tryDeferredQuery`). These
  are blocking shapes whose output is **already projected AND charged** (a set op combines + dedups
  already-projected rows; a `WITH`'s output is its body's), so there is **no per-row top-level projection
  to defer** — the only streaming win is **lazy-yield**: the cursor owns a frozen snapshot engine (§5),
  resolves the output column names by **planning only** up front (unmetered + deterministic, so the names
  match the deferred run's), then on its **first pull** runs the whole eager **`run_set_op` / `run_with`
  verbatim** (so the rows + total cost are byte-identical to `execute()` *by construction* — there is no
  re-implemented execution path to drift) and yields the materialized result one row at a time. A
  `54P01`/`54P02` cost/lifetime abort, a cancellation, or an arithmetic trap surfaces **during
  iteration** (the run is on the first pull), not at `query()` (§6); the snapshot pin registers in the
  watermark like S3/S4. Because the whole query runs on the first pull, an early exit charges the **same**
  as a full drain (the lazy-yield-only nature, unlike S3/S4's early-exit win) — pinned by a per-core unit
  test. A **data-modifying `WITH`** (a write, `stmt_is_write`) and a `nextval`/`setval`-calling set-op/
  `WITH` are **not** taken (they must hold the write gate) — they fall back to the materialized dispatch.
  Verified per core by unit tests (`query()` == `execute()` rows + cost across every set-op kind +
  recursive/aggregate/join `WITH`, the run-fully-on-first-pull cost, the snapshot pin + watermark, the
  mid-drain abort, the data-modifying-`WITH` fallback).
- **S7 — lazy `exec_streaming_sort` output.** ✅ **Landed (all three cores).** The streaming external
  sort (`spec/design/spill.md` §5) buffered its **input** through the `Sorter` already, but its
  **output** was an `Emitter::Final` — the full windowed result `Vec` was built (and `row_produced` +
  the projection charged) up front on the first pull, so an early exit over it charged the **same** as a
  full drain. This slice makes the output lazy: `exec_streaming_sort` runs the **blocking part** (scan +
  sort + the `OFFSET` skip) and returns an **`Emitter::Sorted`** — the `SortedRows` pull iterator
  positioned at the first output row, plus the windowed `remaining` count — and the emitter drive (eager
  in `exec_select_plan`; lazy in `BufferedScan` / `bufferedScanCursor` / a `bufferedRows` generator)
  pulls the next sorted row, charges `row_produced`, and evaluates the projection list **per pull**. So
  the output `Vec` is **never built**, and a caller that stops early skips the `row_produced` +
  projection of every windowed row it never pulls — the **top-N-over-the-sort early-exit win** the
  `Final` form could not offer. The collation-aware path (which cannot use the `C`-ordered `Sorter`)
  wraps its in-memory-sorted survivors as an in-memory `SortedRows`, so it flows through the **same**
  lazy emitter. Under **full drain** the rows + total cost are byte-identical to the eager sort (the same
  `page_read` block, `storage_row_read` per scanned row, filter `operator_eval`, and `row_produced` per
  windowed row — the sort itself is unmetered, cost.md §3; spill.md §6), so the corpus stays green by
  construction. An early exit (or a `LIMIT`-stopped merge) drops the `SortedRows`, whose `Merger`
  cleanup releases any undrained spill run files (Rust on `Drop`; Go via the cursor's `close`; TS via the
  generator's `finally`). Verified per core by unit tests (`query()` == `execute()` rows + cost across a
  battery of sort shapes incl. `OFFSET`/`LIMIT`/projection-expr/filter/empty; the early-exit-charges-less
  win; and the spilling-merge path streaming lazily + leaving no temp file on early exit). This was the
  last `exec_select_emit`-path output-laziness follow-on; the remaining streaming follow-on is a
  `Database::query` watermark on the bare single-handle path (§3/§5) — **prepared-statement streaming
  landed in S8 (below).**
- **S8 — prepared-statement streaming.** ✅ **Landed (all three cores).** A **prepared** query
  (`prepare` + `query_prepared` / `QueryValues` / `PreparedStatement.query`) used to **materialize** —
  it ran the eager `execute`/`dispatch` path and wrapped the resulting `Outcome` in a buffered `Rows`,
  so a prepared `SELECT` got none of S3/S4/S6/S7's laziness (no row-at-a-time pull, no early-exit win,
  no snapshot pin on the session path). The fix is a pure routing change: each core extracts a shared
  **route-an-already-parsed-AST** helper — `query_ast` (Rust `Engine` + `Session`), `queryStmt` (Go
  `engine` + `Session`), `queryStmt` (TS `Engine`) — holding the streaming / buffered / deferred lane
  dispatch (plus, on the `Session` path, the autocommit re-pin and the reader-liveness watermark pin)
  that the ad-hoc `query()` already used. Both `query()` (parse-then-route) and the prepared query
  (route the prepared AST) call it, so **a prepared query now streams identically to a one-shot one** —
  same lazy lanes, same early-exit win, same snapshot pin, the `54P01`/`57014`/trap surfacing **during
  iteration** (the lazy lanes defer their work to the first pull, not to `query_prepared`). Under full
  drain the rows + total cost are byte-identical to the materialized path (the same lanes the corpus's
  `execute()` already exercises), so the corpus stays green by construction; it is per-core unit-tested
  only (`prepared_query_*` / `TestPreparedQuery*` / `"prepared query …"`: matches-eager across every
  lane, binds `$N` params, the early-exit-charges-less win, the session-path snapshot pin + watermark,
  and the mid-drain cost abort). **Cross-core shape note:** Rust/Go expose a low-level prepared query on
  *both* the bare `Engine` and the shared-core `Session` (the latter pins); TS's low-level
  `PreparedStatement` binds only to a bare `Engine` (its session-bound prepared path is the ergonomic
  `Statement`, which already re-parsed + routed through `Session.query`, so it streamed before this
  slice). The **WASM C-ABI** `jed_stmt_query` drains the now-streaming cursor through a new `ok_rows`
  helper that surfaces a mid-drain error as an `ERROR` buffer instead of a silently truncated result
  (the one correctness obligation the streaming change introduces at that boundary); the Ruby native
  extension exposes only `jed_execute` (materialized), so it is unaffected. No `format_version` bump.

Built Rust-first, then Go/TS in lockstep; the streaming loop structure is mirrored across cores (§6).

## 8. The last lazy-decode gap — promoted to its own reshape ([lazy-record.md](lazy-record.md))

The one lazy-decode gap vs. PG/SQLite (§1, §2) is that a leaf decode materializes **every** inline
value of a record, where PG (`slot_getsomeattrs`) and SQLite (`OP_Column`) decode only the columns
referenced (up to the max needed). This was originally specced here as a small "S5" follow-on: jed
already computes the per-relation **touched-column mask** at plan time ([cost.md](cost.md) "The
touched set") and already skips resolving untouched *large* values, so skipping the *decode* of
untouched small inline values reads like a localized codec change behind the existing mask.

**Implementing it surfaced a finding that promoted it from a tweak to a reshape, now specced in
[lazy-record.md](lazy-record.md).** PG and SQLite decode lazily **in the shared page buffer** — the
tuple stays resident and a column is deformed in place, so deferral is free. jed's decoded `Row` is
instead **detached and owned**, and **every scan deep-clones the row out of the resident node**
(`pmap` `Step::Emit(… vals[p].clone())`). So a decode-in-place S5 has nothing to decode in place;
the narrow form is a wash on flat untouched columns and a slight **regression** on the common
touched path, with a clear win only on untouched deep-tree columns (jsonb/array/composite) — for a
larger, drift-prone per-type skip-walker across three cores. The root cause is the **eager decode +
per-scan deep clone**, not the decode-skip.

[lazy-record.md](lazy-record.md) attacks the root: keep a faulted leaf as its **compact on-disk
bytes** and decode each column **on demand** (generalizing the [large-values.md §14](large-values.md)
`Unfetched` path from large values to *every* value). That makes lazy column decode fall out
**uniformly** (no per-type rule) and **for free** (no touched-path regression), and adds two wins
the narrow S5 never reached — the per-scan clone becomes a refcount bump / flat copy instead of a
deep-tree clone, and resident leaf memory drops from inflated `Value` trees to `≈ page_size` (the
honest buffer-pool bound, CLAUDE.md §9). It remains:

- **separate from streaming** (orthogonal; either can land without the other),
- **cost-neutral and byte-neutral** (jed meters no per-column-decode unit and the on-disk format is
  unchanged; the touched-set already governs the `page_read` / `value_decompress` charges, which do
  not move — so each core lands it green independently), and
- a **storage-core reshape, not a localized tweak** — sliced seam-first in
  [lazy-record.md §12](lazy-record.md) (L0 spec, L1 no-construct decode seam, L2 defer-at-fault, L3
  zero-copy block-shared).

## 9. Determinism & cross-core notes (summary)

- **Results + cost are the only contract**, and both are invariant under full drain (§6); the cursor
  shape, the pull scan cursor, and the buffering are internal machinery, **not** a byte contract —
  each core implements them idiomatically (the pager / spill / concurrency precedent).
- **The streaming loop structure is mirrored across cores** so the accrual order — hence the
  deterministic `max_cost` abort point — is identical (§6); this is the one cross-core obligation the
  refactor adds.
- **No nondeterminism leaks.** Row order is still defined only by `ORDER BY` (CLAUDE.md §8); a
  streaming scan emits in primary-key order exactly as the materialized scan did; the snapshot pin
  keys on `txid` (deterministic), never on iteration order.
- **Memory safety holds** — the pull scan cursor is owned-frame traversal in every core (no
  `unsafe`, no cgo; CLAUDE.md §2/§13).
