# Window functions (`OVER`) — design

> The reasoning behind window functions. The **data + grammar are authoritative**
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `over_clause`, `window_definition`,
> `window_clause`, `frame_clause`; [../functions/catalog.toml](../functions/catalog.toml) — the
> `[[window]]` rows, landing with slice S0); this doc is the *why* and — because there is no
> reference implementation (CLAUDE.md §2) — the precise **cross-core contract** every core must
> reproduce: the window stage's position in the pipeline, partition/order/peer determinism,
> result types, NULL / empty-frame behavior, frames, and cost. When a decision here changes,
> change the data/grammar and here in the same edit. PostgreSQL is the behavioral default
> (CLAUDE.md §1); the deliberate divergences are the ledger in §10.

> **Status: COMPLETE (S0–S10, all three cores).** Every function below is landed — row_number,
> rank, dense_rank, percent_rank, cume_dist, ntile, lag, lead, the aggregates as window functions
> (running + explicit `ROWS`/`RANGE`/`GROUPS` frames, including value-based `RANGE` offsets over an
> integer or decimal ordering key, with `EXCLUDE CURRENT ROW`/`GROUP`/`TIES`/`NO OTHERS`),
> first_value/last_value/nth_value, and named windows — **window functions combine with GROUP BY
> / aggregates in one query** (S8): the window stage runs over the grouped rows, an aggregate may sit
> inside a window argument (`sum(sum(x)) OVER ()`), and a window's column keys must be grouping
> columns — and a window definition may **extend a named base window** (S9): `OVER (w ORDER BY …)`
> and `WINDOW w2 AS (w …)` inherit the base's `PARTITION BY` (and its `ORDER BY` if any) and supply
> their own frame (§5) — and a window `ORDER BY` **honors collation** (S10): a per-key `COLLATE` and a
> text column's frozen implicit collation order text by the collation's UCA sort key, driving both the
> per-partition sort and peer determination (§3/§5) — and a window's `PARTITION BY` / `ORDER BY` keys
> are **general expressions** (S11): `PARTITION BY a + b`, `ORDER BY a % 2`, and — in a grouped query —
> an aggregate *as* a key (`ORDER BY sum(x)`), resolved against the grouped row exactly as a
> projection (a non-grouping column is `42803`); a compound key is materialized into a synthetic
> window-key column before the window stage, so the slot-based machinery is unchanged (§5.1). The
> window stage is also **optimized** (cost-lowering only, never correctness — §5.2/§8): specs sharing
> an identical `PARTITION BY` + `ORDER BY` are partitioned and sorted **once**, and a no-`EXCLUDE`
> aggregate **slides** its frame accumulator (an expanding frame folds each row once for every
> aggregate; a moving `count` un-folds the left edge) instead of re-folding each frame. The remaining
> `0A000` items — `RANGE` offsets over a float (divergence D3) / timestamp / date ordering key
> (§6/§11), and a **correlated** window key (an enclosing-query column in a `PARTITION BY` / `ORDER BY`
> — §11) — are deferred follow-ons, not gaps.

A **window function** computes a value for **each row** from a *set* of related rows — its
**window frame** — without collapsing the rows the way an aggregate does. `row_number() OVER
(ORDER BY x)` numbers rows 1, 2, 3…; `sum(x) OVER (ORDER BY t)` is a running total; `rank()
OVER (PARTITION BY g ORDER BY x)` ranks within each group. This is the engine's first construct
that is **per-row *and* a fold over rows** at once — it neither stays per-row (an expression)
nor collapses to one row (an aggregate). It reuses the aggregate resolver's "fold → synthetic
row" machinery ([aggregates.md](aggregates.md) §9) and extends it with a new pipeline stage.

## 1. Role, scope, and staging

A window function is a `function_call` carrying an `OVER` clause
([grammar.md](grammar.md) §51, [grammar.ebnf](../grammar/grammar.ebnf) `over_clause`). The
surface lands in **six vertical slices** (CLAUDE.md §10/§11), each one commit across **all three
cores in lockstep** (Rust + Go + TS), spec-first, with corpus entries + a capability:

0. **S0 — `OVER` + the window stage + `row_number()`.** The "it's alive" milestone: the
   `OVER ([PARTITION BY cols] [ORDER BY …])` clause, the window operator (§5), and the single
   frame-independent function `row_number()`. `PARTITION BY` is narrowed to **columns** (the
   `GROUP BY`/`ORDER BY` narrowing); general expressions are a follow-on. Capability
   `query.window`.
1. **S1 — ranking.** `rank()`, `dense_rank()` (peer-aware, §4), `percent_rank()`, `cume_dist()`
   (→ `decimal`, §10), `ntile(n)` (→ `i64`, `22023` on `n ≤ 0`). All frame-independent.
   Capability `query.window_ranking`.
2. **S2 — offset (frame-independent).** `lag(expr [,offset [,default]])`, `lead(…)` — positional
   within the partition. Capability `query.window_offset`.
3. **S3 — aggregate windows (default frame).** The catalog aggregates `count`/`sum`/`min`/`max`/
   `avg` with an `OVER` clause, under the **implicit default frame only** (running aggregate),
   reusing the existing `Acc` kernels; plus `first_value()`. Explicit frame syntax is still
   `0A000`. Capability `query.window_aggregate`.
4. **S4 — explicit `ROWS` frames.** `ROWS BETWEEN frame_start AND frame_end` (physical row
   offsets, non-negative integer literals), generalizing the frame; `last_value()`,
   `nth_value(expr, n)` (genuinely frame-sensitive). `RANGE`/`GROUPS` and `EXCLUDE` parse but are
   `0A000` at resolve. Capability `query.window_frame`.
5. **S5 — named windows + sharing** *(follow-on)*. The `WINDOW w AS (…)` clause + `OVER w`
   reuse/extension, and the shared partition/sort pass (multiple windows, one sort — a
   cost-relevant optimization, so it carries a NoREC relation + a benchmark). Capability
   `query.window_named`.
6. **S6 — explicit `RANGE`/`GROUPS` frames + value offsets.** `GROUPS BETWEEN …` (peer-group
   integer offsets, requires an `ORDER BY` → `42P20`); `RANGE BETWEEN …` with `UNBOUNDED`/`CURRENT
   ROW` bounds over any ordering (peer/edge based), and value-based `n PRECEDING`/`FOLLOWING`
   offsets over a **single** integer or decimal ordering key (else `42P20`/`0A000`). `CURRENT ROW`
   spans the current peer group in both modes; a NULL ordering key frames only its NULL peers for
   offset/CURRENT bounds. `RANGE` offsets over a float (D3) / timestamp / date key stay `0A000`.
   Capability `query.window_frame_range`.
7. **S7 — frame `EXCLUDE`.** `EXCLUDE CURRENT ROW | GROUP | TIES | NO OTHERS` on any explicit frame:
   after the `[lo, hi)` frame is computed, drop the current row (`CURRENT ROW`), its whole peer
   group (`GROUP`), its peers but not itself (`TIES`), or nothing (`NO OTHERS`, the default). Only
   rows already in the frame are dropped; `first/last/nth_value` pick over the survivors; an
   empty-after-exclusion frame is `NULL` (count `0`). Capability `query.window_frame_exclude`.
8. **S8 — combined with `GROUP BY` / aggregates.** A window function in the SELECT list of a grouped
   (or whole-table aggregate) query: the window stage runs over the *grouped* rows (§2). A window
   function's **arguments** resolve against the grouped row, so a nested aggregate is legal
   (`sum(sum(x)) OVER ()`, `sum(count(*)) OVER ()`); its **`PARTITION BY`/`ORDER BY` column keys must
   be grouping columns** (`42803` otherwise). `HAVING` runs *before* the window stage (a window
   function there is `42P20`); a window function nested in an aggregate argument is `42803`.
   General-expression window keys (an aggregate/expression as a key, `ORDER BY sum(x)`) stay deferred
   (§11). Capability `query.window_grouped`.
9. **S9 — base-window-extending definitions.** A window definition may begin with the name of an
   earlier `WINDOW`-clause entry to **extend** it: `OVER (w ORDER BY …)` and `WINDOW w2 AS (w …)`.
   The extending definition inherits the base's `PARTITION BY` (and its `ORDER BY` if the base has
   one) and supplies its own frame; the rules (all `42P20`, in PostgreSQL's priority order
   PARTITION → ORDER → frame): the extender may not add a `PARTITION BY`, may add an `ORDER BY` only
   if the base has none, and the base must not carry a frame. A base that does not exist — including
   a self- or forward-reference within the `WINDOW` clause — is `42704`. Capability
   `query.window_base_extend`. (See §5 for the full merge contract.)
10. **S10 — collated window `ORDER BY`.** A window `ORDER BY` key honors a per-key `COLLATE` and a
   text column's **frozen implicit collation** (the same `sort_key` production and vendored collations
   as the query `ORDER BY`), ordering text by the collation's UCA sort key; `COLLATE "C"` and an
   uncollated key keep raw-byte / code-point order. The collation drives **both** the per-partition
   sort **and** peer determination — ranking peer groups, the running aggregate default frame, and
   `RANGE`/`GROUPS` frame peer groups (§3/§5) — so a collated window orders, ranks, and frames
   identically cross-core. Because the vendored collations are **deterministic** (collated-equality is
   byte-identity, [collation.md](collation.md) §7), collated peer groups coincide with byte-equal
   groups; only the order changes. `COLLATE` on a non-text key is `42804` (the query `ORDER BY` rule).
   Capability `query.window_collation`.
11. **S11 — general-expression `PARTITION BY` / `ORDER BY` keys.** A window key is a general
   expression, not just a column reference: `PARTITION BY a + b`, `ORDER BY a % 2`, and — in a grouped
   query — an aggregate *as* a key (`ORDER BY sum(x)`, `PARTITION BY g % 2` over a grouping column).
   The keys resolve against the grouped row **exactly as a projection does** (the tie to the GROUP BY
   expression-key): in a plain window query against the input row (`Forbidden`), in a grouped one
   against the grouped row (`Collect`, sharing the query's aggregate specs) — a bare grouping column
   or aggregate is valid, a **non-grouping column is `42803`**; an aggregate inside a window key makes
   the query a whole-table aggregate even with no `GROUP BY` (so `SELECT rank() OVER (ORDER BY sum(x))
   FROM t` is one group, rank 1). A **bare column / aggregate** key keeps its real row slot (so a
   column-only window is byte-identical to before); a **compound** key is materialized into a
   synthetic window-key column evaluated per row before the window stage (`WINDOW_KEY_BASE` placeholder
   → `input_width + k` after rebase), leaving the slot-based partition / sort / frame machinery
   unchanged (§5.1). The materialized key expression is metered like any expression (`operator_eval`
   per node) — new, deterministic, cross-core-identical work that exists only for an expression key.
   A key referencing an **enclosing-query column** (a correlated window) is `0A000` (§11). Capability
   `query.window_expr_keys`.

Locked scope decisions: **the within-partition order is always fully resolved** (§3,
deterministic — a divergence-adjacent strictness, §10); **`percent_rank`/`cume_dist` →
`f64`** (PG's `float8`, §4 — the in-contract correctly-rounded division); **`PARTITION BY` columns
only** in S0; **explicit `ROWS` frames in S4, `RANGE`/`GROUPS` + value offsets in S6** (S0–S3 use
the implicit default frame, §6).

## 2. Pipeline position — where the window stage runs

Window functions evaluate over the result of grouping and *before* the final presentation
clauses — the PostgreSQL order (CLAUDE.md §1):

```
scan → WHERE → GROUP BY / HAVING → ★ WINDOW ★ → DISTINCT → ORDER BY → LIMIT / OFFSET
```

Two consequences are load-bearing:

- **Window functions see post-aggregation rows** (S8). In a grouped query, a window function runs
  over the grouped synthetic rows, so its arguments resolve against `[group_keys…, agg_results…]` —
  an aggregate *inside* a window argument is legal (`sum(sum(x)) OVER ()`, `sum(count(*)) OVER ()`),
  and its `PARTITION BY`/`ORDER BY` **column** keys must be **grouping columns** (a non-grouping
  column anywhere in a window construct is `42803`). An aggregate or general expression *as* a window
  key (`rank() OVER (ORDER BY sum(x))`) is the deferred general-expression-key follow-on (§11). A
  window function may **not** appear in `WHERE`, a `JOIN ON`, `GROUP BY`, `HAVING`, or another window
  function's `PARTITION BY`/`ORDER BY`/frame bound (those run *before* the window stage) — that is
  `42P20` (§7), the windowing analog of the aggregate's `42803` ([aggregates.md](aggregates.md) §6);
  a window function nested in an *aggregate's* argument is `42803` (an aggregate cannot fold a window
  result, since the window stage runs after aggregation).
- **`DISTINCT`/`ORDER BY`/`LIMIT` see post-window rows**, so a query may `ORDER BY` or filter on
  a `row_number()` (via a wrapping subquery; a window function in the *same* query's `WHERE` is
  still `42P20` — push it down a level, exactly as PostgreSQL requires).

The window stage is a **blocking operator** (it must see every input row before emitting any),
like `ORDER BY` and the aggregate bucketer — so its partition + per-partition sort are
**unmetered** ([cost.md](cost.md) §3, §8 below). Under the spill follow-on it becomes a
spilling sort (the [spill.md](spill.md) external-merge `Sorter` is reusable). The window stage
adds **no on-disk format change** (`format_version` unchanged — all state is in-memory, the
temp-table precedent) and **no key encoding**.

## 3. The window definition — partition, order, and resolved order

A `window_definition` is `[name] [PARTITION BY …] [ORDER BY …] [frame]`
([grammar.ebnf](../grammar/grammar.ebnf)). Its three parts:

- **`PARTITION BY`** splits the input into **partitions** that share a value on every partition
  key. Partitions are independent — a window function restarts at each partition boundary.
  Keys bucket by **value-canonical** form (so `1.5` and `1.50` partition together,
  [decimal.md](decimal.md) §5), `NULL` is its own partition (`NULL` partitions with `NULL`, not
  three-valued), and the partition list is **insertion-ordered, never a hash-map iteration** —
  the aggregate-grouping discipline (no §8 iteration-order leak). With no `PARTITION BY` the
  whole (post-group) result is one partition.
- **`ORDER BY`** orders rows *within* each partition. It is the same `sort_key` production as the
  query `ORDER BY` (per-key `ASC`/`DESC`, `NULLS FIRST|LAST`, `COLLATE`), narrowed in S0 to
  columns. A `COLLATE` and a text column's frozen implicit collation are **honored** (S10): text
  orders by the collation's UCA sort key, both in the sort and in peer determination (§5.2);
  `COLLATE "C"` / an uncollated key keep raw-byte order, and `COLLATE` on a non-text key is `42804`.
- **The frame** (§6) — deferred to S4; S0–S3 use the implicit default.

**The resolved within-partition order (the determinism rule).** A window function's per-row
value can depend on row sequence (`row_number`, `lag`, frame position), so the sequence must be
**fully determined**. jed defines the effective within-partition order as **the window `ORDER
BY` keys, then the primary key** (the same stable PK tie-break the query `ORDER BY` uses,
[order_by.test](../conformance/suites/query/order_by.test)). Absent a window `ORDER BY`, the
order is **primary-key (storage scan) order**. So `row_number() OVER ()` is deterministic and
byte-identical cross-core. PostgreSQL leaves this *unspecified*; jed pins it — a deliberate
strictness consistent with the §8 "no iteration-order leak" rule and the value-canonical grouping
of aggregates (§10, ledger D1).

**Peer rows vs. sequence (the one subtle point).** Two rows are **peers** when they are equal on
the window `ORDER BY` keys **only** — the PK tie-break orders peers into a sequence but does
**not** split a peer group. The distinction matters:

- `row_number()` uses the **sequence** (PK-tie-broken): every row gets a distinct number, peers
  ordered by PK.
- `rank()`/`dense_rank()`/`percent_rank()`/`cume_dist()` use **peers**: peers share a rank.
- `RANGE`/`GROUPS` frames (§6) treat peers as one unit; `ROWS` frames use the sequence.

With no window `ORDER BY` every row is a single peer group, so `rank()` = `1` for all rows, and a
`RANGE` default frame spans the whole partition.

## 4. The window-function contract

Each function's result type, frame sensitivity, and NULL behavior, as data in the `[[window]]`
catalog array (S0+) and reused `[[aggregate]]` rows (S3):

| Function | Args | Result | Frame? | Needs order? | NULL / empty | Slice |
|---|---|---|---|---|---|---|
| `row_number()` | — | `i64` | no | no | never | S0 |
| `rank()` | — | `i64` | no | no | never | S1 |
| `dense_rank()` | — | `i64` | no | no | never | S1 |
| `percent_rank()` | — | `f64` | no | no | never | S1 |
| `cume_dist()` | — | `f64` | no | no | never | S1 |
| `ntile(n)` | `n int` | `i64` | no | no | never (`n ≤ 0` → `22003`†) | S1 |
| `lag(e [,off [,def]])` | any | input type | no | no | NULL if offset leaves partition (no default) | S2 |
| `lead(e [,off [,def]])` | any | input type | no | no | NULL if offset leaves partition (no default) | S2 |
| `count`/`sum`/`min`/`max`/`avg … OVER` | (aggregate) | (aggregate widening) | **yes** | no | aggregate rule over the frame | S3 |
| `first_value(e)` | any | input type | **yes** | no | NULL if frame empty | S3 |
| `last_value(e)` | any | input type | **yes** | no | NULL if frame empty | S4 |
| `nth_value(e, n)` | any, int | input type | **yes** | no | NULL if frame has `< n` rows (`n < 1` → `22023`) | S4 |

† `ntile(0)`/`ntile(-1)` raise **`22023`** (`invalid_parameter_value`) — PG's
`invalid_argument_for_ntile`; jed reuses the existing `22023` (no message match, [conformance.md](conformance.md) §2).

**Result-type notes.**

- **Ranking counters** (`row_number`/`rank`/`dense_rank`/`ntile`) → `i64`, exact, matching PG's
  `bigint`/`int4` (jed widens `ntile` to `i64` — its own integer narrowing rule).
- **`percent_rank`/`cume_dist` → `f64`** (PG's `float8`). They are ratios —
  `percent_rank = (rank − 1) / (N − 1)` (`0` when `N = 1`, per PG: a lone row has `percent_rank`
  `0`), `cume_dist = (# rows ≤ current peer) / N` — computed as **one IEEE correctly-rounded `f64`
  division**. The numerator and denominator are small partition counts that convert *exactly* to
  binary64 (≤ 2^53), and binary64 `/` is IEEE-mandated correctly-rounded, so the result is
  **bit-identical across cores and to PostgreSQL** — the in-contract float kernel
  ([float.md](float.md) §5), no exemption and no oracle override. (This formerly returned `decimal`
  via the exact division — divergence D2 — kept solely to keep binary floats out of the value path;
  with the `f64` type landed, returning `float8` removes the divergence at no determinism cost.)
- **Value functions** (`lag`/`lead`/`first_value`/`last_value`/`nth_value`) → **the value
  expression's type** (the `same_as_input` reserved id, [functions.md](functions.md) §8). `lag`'s
  `default` argument, if present, must be assignable to that type (`42804` otherwise).
- **Aggregate windows** inherit every aggregate widening *and divergence* unchanged: `SUM(i16|i32)`
  → `i64`, `SUM(i64)` → `decimal`, `AVG` → `decimal`, the order-independent canonical float fold
  for `SUM(float) OVER` ([float.md](float.md) §7), and the empty-frame → `NULL` rule
  ([aggregates.md](aggregates.md) §2).

## 5. The executor stage — resolver split + the window operator

### 5.1 Resolver split (a `WindowCtx`, layered after `AggCtx`)

The model generalizes `AggCtx::Collect { group_keys, specs }` and its synthetic row
`[group_keys…, agg_results…]` ([aggregates.md](aggregates.md) §9). The window pass runs **after**
the aggregate pass and **extends** the synthetic row:

```
synthetic row  =  [ group_keys… , agg_results… , window_results… ]
                  └────────── post-GROUP-BY ─────┘└── post-WINDOW ──┘
```

- A `WindowCtx::Collect { specs }` collects each `FuncCall` carrying an `OVER` clause into a
  `WindowSpec` and resolves the call to a **synthetic slot** at
  `group_keys.len() + agg_specs.len() + window_index`. The projection then evaluates positionally
  with the *existing* expression evaluator — **no new expression node** (the exact aggregate
  trick).
- A `WindowSpec` carries: the `WindowPlan` (an enum paralleling `AggPlan` —
  `RowNumber`/`Rank`/`DenseRank`/`PercentRank`/`CumeDist`/`Ntile`/`Lag`/`Lead`/`Agg(AggPlan)`/
  `FirstValue`/`LastValue`/`NthValue`), the resolved `partition_keys`, the resolved `order_keys`
  (with the PK tie-break appended, §3), the resolved frame (§6), and the resolved argument
  `RExpr`s.
- **Argument *and key* resolution scope** (S8 / S11). A window function's arguments **and its
  `PARTITION BY` / `ORDER BY` keys** resolve in the *same* sub-context: against the raw scan row
  (`Forbidden`) in a non-grouped query, or against the grouped synthetic row `[group_keys…,
  agg_results…]` (`Collect`, sharing the query's agg specs) in a grouped one. So an aggregate nested in
  an argument collects (`sum(sum(x)) OVER ()`), a bare non-grouping column is `42803`, and — S11 — a
  **key is a general expression** resolved the same way: `PARTITION BY a + b`, `ORDER BY a % 2`, and an
  aggregate *as* a key (`ORDER BY sum(x)`, collected like any aggregate → its agg slot). A window
  function in `WHERE`/`HAVING`/`GROUP BY`/another window's key, or nested in another window function,
  is `42P20`; a window function nested in an *aggregate's* argument is `42803`; a key referencing an
  enclosing-query column (a correlated window) is `0A000`.
- **Window-key materialization** (S11). Each resolved key is mapped to a window-stage slot: a **bare
  column / aggregate** (an `RExpr::Column`) keeps its real row slot — so a column-keyed window is
  byte-identical to before, no extra column, no extra cost; a **compound** key (`a + b`, `a % 2`,
  `sum(x) + 1`) is collected into a query-global `window_keys` list at a `WINDOW_KEY_BASE`
  placeholder slot. Before the window stage each row evaluates the `window_keys` and **appends** the
  values, so a materialized key reads slot `input_width + k` and the slot-based partition / sort /
  frame machinery is unchanged; the key evaluation is metered like any expression (`operator_eval` per
  node). The synthetic row therefore grows to `[group_keys… , agg_results… , window_keys… ,
  window_results…]`.
- Because neither the materialized-key count nor (in a grouped query) the final aggregate count is
  known until resolution finishes, both a window result and a materialized window key are resolved to
  a **placeholder slot** (`WINDOW_RESULT_BASE + w` / `WINDOW_KEY_BASE + k`) and rebased once the row
  layout is final — results to `input_width + window_keys.len() + w`, keys to `input_width + k`. With
  no compound keys `window_keys` is empty and a result lands at `input_width + w`, exactly as before.

### 5.2 The window operator

A blocking stage between projection/aggregation and `DISTINCT`/`ORDER BY`:

1. **Materialize** the input rows (post-WHERE/GROUP-BY/HAVING) into a buffer — the stage is
   blocking by nature; under the spill follow-on it becomes a spilling sort
   ([spill.md](spill.md)). Then **materialize the compound window keys** (S11): for each buffered row,
   evaluate every `window_keys` expression and append the values, so a materialized key reads slot
   `input_width + k`. Empty (a no-op) for a column-only window.
2. For each **distinct window definition** (`partition_keys` + `order_keys` + frame): **partition**
   the buffer (value-canonical keys, an insertion-ordered partition list — §3), and **sort** each
   partition by `order_keys` with the PK tie-break. **Specs that share an identical `partition_keys`
   + `order_keys` are partitioned and sorted ONCE** (`group_window_specs` — the shared partition/sort
   pass), and each then computes its own results over the shared sorted partitions; the partition +
   sort are the expensive, **unmetered** step (§8), so sharing them lowers wall-clock without changing
   the cost or the rows (`group_window_specs` is conservative — identical, not yet PostgreSQL's
   prefix-compatible, definitions; §11).
   When an `order_key` is **collated** (S10), the spec's collated UCA sort-key bytes are decorated
   **once per row up front** (the query `ORDER BY`'s decorate-sort pattern; an unmapped code point is
   `0A000` at that deterministic per-row point), and one collation-aware comparator drives the sort
   **and** every peer determination below — ranking peer groups, the running aggregate default frame,
   and `RANGE`/`GROUPS` frame peer groups — so the collation never diverges between ordering and
   peering. The sort and partition stay **unmetered** (§8); collation adds no cost unit here.
3. For each spec, walk each partition in resolved order and write the per-row result into the
   spec's synthetic slot:
   - **`RowNumber`** → 1-based sequence position.
   - **`Rank`** → 1 + (# rows in earlier peer groups); **`DenseRank`** → 1 + (# earlier peer
     groups). Peers per §3 (`order_keys` equality only).
   - **`PercentRank`/`CumeDist`** → the `f64` ratios (§4, the in-contract correctly-rounded
     division); `Ntile(n)` → the bucket index by the PG distribution rule (larger buckets first).
   - **`Lag`/`Lead`** → the value-expression of the row `offset` positions back/forward in the
     partition sequence, else the `default` (or `NULL`).
   - **`Agg(plan)`** → reuse the existing `Acc` ([executor.rs `Acc`]) folded over the row's
     **frame** (§6) rather than the whole group. S3: the implicit default frame; S4+ the explicit
     frame. For a no-`EXCLUDE` explicit frame the accumulator is **carried** across rows (the
     sliding-window optimization, §6/§8) — an expanding frame folds each row once, a moving
     `count`/`count(*)` additionally un-folds the left edge (`Acc::unfold`) — instead of re-folding
     each frame; a moving `sum`/`avg`/`min`/`max`/float and any `EXCLUDE` frame re-fold from scratch.
   - **`FirstValue`/`LastValue`/`NthValue`** → the value-expression of the first/last/nth row of
     the **frame**.
4. The per-spec **finalize** (the `percent_rank`/`cume_dist`/`avg` division, the `Acc` finalize)
   is **unmetered**, like `AVG`'s division today.

### 5.3 Named windows and base-window extension (S5 / S9)

The `WINDOW name AS ( … )` clause names reusable window definitions; an `OVER name` reference reuses
one (S5), and a definition may **extend** an earlier one by naming it as a leading **base** (S9).
Both are handled **before resolution**, by rewriting the AST into all-inline definitions — the
window operator (§5.2) never sees a name or a base. Two passes, in this order:

1. **Resolve the `WINDOW` clause** (`resolve_window_clause`). Each entry is processed left-to-right;
   an entry that names a base extends an **already-resolved earlier** entry. Every entry is resolved,
   **even one no `OVER` references** — matching PostgreSQL's whole-clause check. The output is a list
   of inline definitions (no remaining base).
2. **Desugar the select-list references** (`desugar_named_windows`) against that resolved list: a
   pure `OVER name` copies the named definition **whole, frame included** (no merge rules); an inline
   `OVER (base … )` is **merged** onto its named base.

**The merge contract** (`extend_window`, PostgreSQL `transformWindowDefinitions`). An extending
definition inherits the base's `PARTITION BY`, and the base's `ORDER BY` when the base has one, and
supplies its **own** frame. The three rules fire in PostgreSQL's priority order and all raise
`42P20` (`windowing_error`):

| # | Condition | Error |
|---|---|---|
| 1 | the extender adds a `PARTITION BY` (even one identical to the base's) | `cannot override PARTITION BY clause of window "<b>"` |
| 2 | the base has an `ORDER BY` **and** the extender adds one | `cannot override ORDER BY clause of window "<b>"` |
| 3 | the base carries a frame (even for a bare `OVER (b)`) | `cannot copy window "<b>" because it has a frame clause` |

A base that does not exist — including a **self- or forward-reference** within the `WINDOW` clause,
since the base must be an *earlier* entry — is `42704` (`undefined_object`), the same code as a
missing `OVER name`. The crucial distinction: the merge rules apply only to the **parenthesized
extend** form (`OVER (b … )` / `WINDOW w AS (b … )`). A bare **`OVER b`** is a pure reference that
copies `b` whole, so a framed `b` is fine via `OVER b` but `42P20` via `OVER (b)` — oracle-verified.

## 6. Frames

A frame restricts the rows a frame-sensitive function (the aggregates, `first/last/nth_value`)
folds over, *per current row*. The frame is `{ROWS | RANGE | GROUPS} frame_extent
[frame_exclusion]` ([grammar.ebnf](../grammar/grammar.ebnf) `frame_clause`).

- **The default frame** (S3+, the only frame in S0–S3). With a window `ORDER BY`:
  `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` — every row from the partition start
  through the current row's peer group (a **running** aggregate). With no `ORDER BY`:
  the **whole partition** (`ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING`). This is
  PG's default and what makes `sum(x) OVER (ORDER BY t)` a running total but `sum(x) OVER ()` a
  partition total.
- **`ROWS`** — physical row offsets in the partition sequence (`ROWS BETWEEN 2 PRECEDING AND
  CURRENT ROW` is a 3-row sliding window).
- **`RANGE`** — logical offsets on the *single* `ORDER BY` key value (`RANGE BETWEEN 5
  PRECEDING AND 5 FOLLOWING`). `CURRENT ROW` in `RANGE` means the current row's **peer group**;
  with only `UNBOUNDED`/`CURRENT ROW` bounds (no value offset) it is peer/edge based and works over
  **any** number of `ORDER BY` keys (or none → one peer group). A value offset (`n PRECEDING`/
  `FOLLOWING`) frames the rows whose key is within `n` of the current key and requires **exactly
  one** `ORDER BY` key (else `42P20`) of an **integer** (integer offset) or **decimal** (integer/
  decimal offset) type; an integer key with a decimal offset, a float key (divergence D3), a
  timestamp/date key (deferred), or any other type is **`0A000`**. A **NULL** current key frames
  only its NULL peers for offset/`CURRENT ROW` bounds, while `UNBOUNDED` bounds still reach the
  partition edge (matching PG). Integer bound arithmetic is exact (i128 / bigint, so it never
  overflows — matching PG's saturating frame edge).
- **`GROUPS`** — peer-group offsets (`GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW`). A bound `g
  PRECEDING`/`FOLLOWING` lands on the `cg ∓ g`-th peer group's start/end. `GROUPS` requires an
  `ORDER BY` (else `42P20`).
- **`EXCLUDE CURRENT ROW | GROUP | TIES | NO OTHERS`** — frame exclusion (S7). Computed *after*
  `[lo, hi)`: it drops the current row, its whole peer group, its peers but not itself, or nothing
  (the default). It removes only rows already inside the frame, so `EXCLUDE CURRENT ROW` on a frame
  the current row is not part of is a no-op. `first/last/nth_value` pick over the survivors;
  an empty-after-exclusion frame is `NULL` (count `0`). The metered `window_frame_step` is charged
  only for surviving folded rows. Works with every frame mode and the default (`None`) frame is
  always `NO OTHERS` (no clause ⇒ no exclusion).

S4 shipped explicit `ROWS BETWEEN frame_start AND frame_end`; **S6** added `RANGE`/`GROUPS` and
value-based `RANGE` offsets (integer/decimal keys); **S7** added `EXCLUDE`. `RANGE` offsets over a
float (D3) / timestamp / date key stay `0A000`. A frame bound that contains a window function, an
aggregate, a column reference, or a negative offset is rejected (`42P20`/`42803`/`0A000`/`22013` as
appropriate, matching PG); a malformed `EXCLUDE` is `42601`.

Frame evaluation **slides** for a no-`EXCLUDE` aggregate (§5.2/§8): the sorted partition makes the
frame bounds `[lo, hi)` monotonic non-decreasing, so the accumulator is carried across rows — an
**expanding** frame (start `UNBOUNDED PRECEDING`) folds each row once (every aggregate, byte-identical
because the fold order is the sorted-prefix order the naive path uses), a **moving** frame additionally
un-folds the left edge for the invertible `count`/`count(*)`; a moving `sum`/`avg`/`min`/`max`/float
re-folds from scratch when `lo` advances (the naive O(partition²) — decimal scale, the
intermediate-overflow trap order, and float non-associativity make them unsafe to invert), as does
any frame with `EXCLUDE`. The cost meter (§8) still bounds the work so an untrusted running-window
query aborts on `max_cost`.

## 7. Where a window function may not appear (`42P20`)

A window function runs in the dedicated window stage (§2), so it is rejected anywhere that runs
before it or per input row — PostgreSQL's SQLSTATE `42P20` (`windowing_error`):

- **`WHERE`, a `JOIN ON`, `GROUP BY`, `HAVING`, a `CHECK`/`DEFAULT`** — these run before (or
  without) the window stage.
- **Nested in another window function**, or in another window's `PARTITION BY`/`ORDER BY`/frame
  bound.
- **A window-only function used without `OVER`** — `row_number()` with no `OVER` is **`42809`**
  (`wrong_object_type`, "window function row_number requires an OVER clause" — PG's code here,
  oracle-verified, *not* the `42P20` PG uses for a window function in WHERE/HAVING); an *aggregate*
  without `OVER` is simply an ordinary aggregate.
- **`OVER w` naming a non-window** (a `WINDOW` name that doesn't exist) — `42704`; `OVER` on a
  scalar/SRF function — `42P20`.

Matching is on the **code**, not the message ([conformance.md](conformance.md) §2), so one code
covers every site with site-specific message detail. (The aggregate analog is `42803`,
[aggregates.md](aggregates.md) §6.)

## 8. Cost accrual (the cross-core contract — [cost.md](cost.md) §3)

Two new units; the partition + per-partition sort are **unmetered** (the `ORDER BY`/`GROUP BY`
precedent — their input cardinality is already bounded by upstream `storage_row_read`/
`row_produced`):

- **`window_result`** (weight 1) — once per `(input row × window function)` result materialized
  into the synthetic row; the window-stage analog of `aggregate_accumulate`.
- **`window_frame_step`** (weight 1) — once per frame row folded **or un-folded** in a
  frame-sensitive function's accumulator. The **sliding-window optimization** (§5.2/§6) lowers it:
  a no-`EXCLUDE` aggregate carries one accumulator across the sorted partition, charging one step
  per row **entering** the frame on the right (`fold`) plus, for the invertible `count`/`count(*)`,
  one per row **leaving** on the left (`unfold`) — so an expanding frame is O(n) (each row folded
  once) and a moving `count` is O(n) (each row folded + un-folded once). A moving
  `sum`/`avg`/`min`/`max`/float (re-fold from scratch when `lo` advances) and any frame with
  `EXCLUDE` keep the naive per-row re-fold (O(partition²) worst case). The optimization only
  **lowers** this count — and the operand's `operator_eval`, since each folded row's operand is
  evaluated once and cached — never raising either and never changing the result. Charged at the
  identical per-fold/un-fold point in every core, so the count stays cross-core identical (§8/§13)
  and a `max_cost` ceiling still aborts an untrusted running-window query deterministically (`54P01`).
- **Reused unchanged**: `storage_row_read` per scanned input row; the window arguments' `operator_eval`s
  per input row; `row_produced` per emitted row; the projection's `operator_eval`s per emitted row.
- **Window-key materialization** (S11, §5.1): a **compound** `PARTITION BY`/`ORDER BY` key
  (`a + b`, `a % 2`) is evaluated once per input row and charges `operator_eval` per node, like any
  expression — so an expression key adds metered work that a bare-column key does not. Deterministic
  and cross-core identical (the same `RExpr` evaluated the same way in every core); a bare-column key
  materializes nothing and is byte-identical to before.
- **Unmetered**: the partition bucketing, the per-partition sort, and each spec's finalize (the
  ratio/`avg` division) — like the `ORDER BY` sort and the `DISTINCT` dedup.

So a `row_number()`-only query over `N` rows accrues `N` (`storage_row_read`) `+ N`
(`window_result`) `+ N` (`row_produced`) `+ page_read` — pinned in the corpus with the `# cost:`
directive. A `sum(x) OVER (ORDER BY t)` running total over `N` rows adds the frame fold:
`N` (`window_result`) `+ Σ frame sizes` (`window_frame_step`) — for the running default frame,
`1 + 2 + … + N` per partition.

## 9. Determinism (CLAUDE.md §8/§10)

- **Fully resolved order** (§3): the within-partition sequence is `order_keys` then PK, always
  total, so every window value is byte-identical cross-core even with no window `ORDER BY`
  (ledger D1).
- **Insertion-ordered partitions**: no hash-map iteration order leaks into which rows partition
  together or the per-partition results — every core iterates an explicit insertion-ordered list
  (the aggregate-grouping discipline). Emission order with no query `ORDER BY` stays unspecified
  (the corpus compares `rowsort` or adds an `ORDER BY`); the *values* are deterministic.
- **The `f64` ratios** (`percent_rank`/`cume_dist`) are one IEEE correctly-rounded division of
  small exactly-representable integer counts ([float.md](float.md) §5), so the value is
  bit-identical across cores and to PostgreSQL — in-contract, no exemption; the `R` render tag
  ([conformance.md](conformance.md) §1) absorbs any cross-core layout difference.
- **Aggregate-window reuse** inherits the aggregate determinism contract unchanged (the
  order-independent float fold, the widening overflow boundaries).

## 10. Divergence ledger (CLAUDE.md §1/§8 — recorded, oracle-overridden)

Deliberate divergences from PostgreSQL, each registered in
[../conformance/oracle_overrides.toml](../conformance/oracle_overrides.toml):

- **D1 — within-partition order is pinned, not unspecified.** Absent a window `ORDER BY`, jed
  orders a partition by primary-key/scan order (§3); PG leaves it unspecified. jed is deliberately
  stricter on determinism (the §8 no-iteration-leak rule). Observable only for functions sensitive
  to peer/row sequence with no `ORDER BY` (e.g. `row_number() OVER (PARTITION BY g)`).
- **~~D2 — `percent_rank`/`cume_dist` → `decimal`, not `float8`~~ (RESOLVED — no longer a
  divergence).** These now return `f64`, matching PG's `float8` exactly (§4): the ratio is one IEEE
  correctly-rounded division of small exactly-representable integer counts, so jed's value is
  bit-identical to PostgreSQL's and `window/ratio.test` is oracle-clean (no override). The original
  divergence existed only to keep binary floats out of the value path before the `f64` type landed;
  with `f64` available, removing the divergence costs no determinism (D-numbering kept stable —
  D1/D3/D4 unchanged).
- **D3 — `float`-keyed `RANGE`-offset frames are `0A000`.** A `RANGE BETWEEN n PRECEDING` needs
  `order_key ± n` over the single ordering key; over `float` that re-imports float ordering into a
  comparison path, so it is refused (matching the float-PK `0A000` and the date strict-island
  precedents). PG supports `float8` RANGE offsets; jed's `0A000` is oracle-overridden.
  `ROWS`/`GROUPS` frames over a float key are fine (no key arithmetic).
- **D4 — `timestamp`/`timestamptz`/`date`-keyed `RANGE`-offset frames are `0A000` (deferred).** PG
  supports an `interval` offset over a timestamp key (and the standard's `'1 day' PRECEDING`); jed
  defers the timestamp/date families (only integer/decimal ordering keys take a value offset this
  slice), so they are `0A000` — a deferred follow-on, not a permanent refusal like D3. (A `date`
  key with an *integer* offset is `0A000` in PG too, so only the interval-offset shapes diverge.)

## 11. Deferred / out of scope

- **`FILTER (WHERE …)`** on an aggregate window, and `WITHIN GROUP` ordered-set/hypothetical-set
  window functions (`rank() WITHIN GROUP`, `percentile_cont`) — additive later features (the
  aggregate `FILTER` follow-on, [aggregates.md](aggregates.md) §10).
- **General-expression `PARTITION BY`/`ORDER BY`** — ✅ **landed (S11).** `PARTITION BY a + b`,
  `ORDER BY a % 2`, and — in a grouped query — an aggregate *as* a window key (`ORDER BY sum(x)`),
  resolved against the grouped row like a projection (§5.1). The one remaining piece is a **correlated**
  window key — an enclosing-query column in a `PARTITION BY`/`ORDER BY` (`(SELECT … OVER (PARTITION BY
  outer.k) …)`) — which stays `0A000` (the general-expression-key follow-on stops at the current query
  level; PG supports a correlated window inside a subquery, an oracle-overridden divergence in
  `window/expr_keys.test`).
- **`RANGE` value offsets over a timestamp/timestamptz/date key** (an `interval`/integer offset, D4)
  — deferred; only integer/decimal ordering keys take a value offset this slice. A float key stays
  `0A000` permanently (D3). Non-literal/expression frame offsets are also out (literals only, like
  the `ROWS` narrowing).
- **Wider sharing / sliding** — *cost-lowering only, never correctness; the landed core is §5.2/§8.*
  The shared partition/sort pass groups specs with an **identical** partition+order; PostgreSQL's
  prefix-**compatible** sharing (a shorter `ORDER BY` reusing a longer one's sort) is not yet done.
  The frame slide carries the accumulator for an expanding frame (every aggregate) and a moving
  `count`/`count(*)` (the invertible un-fold); a **moving `sum`/`avg`/`min`/`max`/float still re-folds
  from scratch** — a safely-invertible decimal/float sum (guarding the result scale, the
  intermediate-overflow trap order, and float associativity) is the open follow-on.
- **`IGNORE NULLS`** on `lag`/`lead`/`first_value`/… (SQL:2011, PG does not support it) — out.
