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

> **Status: COMPLETE (S0–S9, all three cores).** Every function below is landed — row_number,
> rank, dense_rank, percent_rank, cume_dist, ntile, lag, lead, the aggregates as window functions
> (running + explicit `ROWS`/`RANGE`/`GROUPS` frames, including value-based `RANGE` offsets over an
> integer or decimal ordering key, with `EXCLUDE CURRENT ROW`/`GROUP`/`TIES`/`NO OTHERS`),
> first_value/last_value/nth_value, and named windows — **window functions combine with GROUP BY
> / aggregates in one query** (S8): the window stage runs over the grouped rows, an aggregate may sit
> inside a window argument (`sum(sum(x)) OVER ()`), and a window's column keys must be grouping
> columns — and a window definition may **extend a named base window** (S9): `OVER (w ORDER BY …)`
> and `WINDOW w2 AS (w …)` inherit the base's `PARTITION BY` (and its `ORDER BY` if any) and supply
> their own frame (§5). The remaining `0A000` items — `RANGE` offsets over a float (divergence D3) /
> timestamp / date ordering key (§6/§11), and **general-expression window keys** (an aggregate or
> expression as a `PARTITION BY`/`ORDER BY` key, e.g. `ORDER BY sum(x)` — §11) — are deferred
> follow-ons, not gaps.

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

Locked scope decisions: **the within-partition order is always fully resolved** (§3,
deterministic — a divergence-adjacent strictness, §10); **`percent_rank`/`cume_dist` →
`decimal`**, not PG's `float8` (§10, the `AVG`→`decimal` precedent); **`PARTITION BY` columns
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
  columns.
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
| `percent_rank()` | — | `decimal` | no | no | never | S1 |
| `cume_dist()` | — | `decimal` | no | no | never | S1 |
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
- **`percent_rank`/`cume_dist` → `decimal`**, *not* PG's `float8` (ledger D2, §10). They are
  ratios — `percent_rank = (rank − 1) / (N − 1)` (1.0 when `N = 1`… actually `0` when `N = 1`,
  per PG: a lone row has `percent_rank` `0`), `cume_dist = (# rows ≤ current peer) / N` — computed
  through the **exact decimal division** `select_div_scale` + half-away rounding, the *same*
  machinery as `AVG` ([decimal.md](decimal.md) §4), the engine's hardest cross-core path. This
  keeps binary floats out of the value/output path (§8) and is consistent with `AVG`/`SUM`→`decimal`.
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
- **Argument resolution scope** (S8). In a non-grouped query, a window function's arguments and keys
  resolve against the raw scan row. In a grouped query the **arguments** resolve against the grouped
  synthetic row `[group_keys…, agg_results…]` — an aggregate nested in an argument collects into the
  query's shared agg specs (`sum(sum(x)) OVER ()`), and a bare non-grouping column is `42803` — while
  the `PARTITION BY`/`ORDER BY` **column** keys map to their grouping-column slot (`42803` if not a
  grouping column). A window function in `WHERE`/`HAVING`/`GROUP BY`/another window's key, or nested
  in another window function, is `42P20`; a window function nested in an *aggregate's* argument is
  `42803`. Because the real window-result slot (`group_keys.len() + agg_specs.len() + w`) is not known
  until every aggregate is collected (one may sit in a later window argument or in `HAVING`), a window
  result is resolved to a placeholder slot and rebased to its real slot after resolution finishes.

### 5.2 The window operator

A blocking stage between projection/aggregation and `DISTINCT`/`ORDER BY`:

1. **Materialize** the input rows (post-WHERE/GROUP-BY/HAVING) into a buffer — the stage is
   blocking by nature; under the spill follow-on it becomes a spilling sort
   ([spill.md](spill.md)).
2. For each **distinct window definition** (`partition_keys` + `order_keys` + frame): **partition**
   the buffer (value-canonical keys, an insertion-ordered partition list — §3), and **sort** each
   partition by `order_keys` with the PK tie-break. In S0–S4 each `WindowSpec` may do its own
   pass; S5 shares one pass across specs with an identical definition (the optimization).
3. For each spec, walk each partition in resolved order and write the per-row result into the
   spec's synthetic slot:
   - **`RowNumber`** → 1-based sequence position.
   - **`Rank`** → 1 + (# rows in earlier peer groups); **`DenseRank`** → 1 + (# earlier peer
     groups). Peers per §3 (`order_keys` equality only).
   - **`PercentRank`/`CumeDist`** → the exact-decimal ratios (§4); `Ntile(n)` → the bucket index
     by the PG distribution rule (larger buckets first).
   - **`Lag`/`Lead`** → the value-expression of the row `offset` positions back/forward in the
     partition sequence, else the `default` (or `NULL`).
   - **`Agg(plan)`** → reuse the existing `Acc` ([executor.rs `Acc`]) folded over the row's
     **frame** (§6) rather than the whole group. S3: the implicit default frame; S4: the explicit
     frame.
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

Frame evaluation is naive (re-fold per row, O(partition²) worst case) until the S5 sliding-window
optimization; the cost meter (§8) bounds it so an untrusted running-window query still aborts on
`max_cost`.

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
- **`window_frame_step`** (weight 1) — once per frame row folded into a frame-sensitive function's
  accumulator. Bounds the per-frame work a naive O(partition²) frame scan can drive, so a
  `max_cost` ceiling aborts an untrusted running-window query deterministically (`54P01`, §13).
  The S5 sliding-window optimization only **lowers** this count; it never changes correctness or
  the result.
- **Reused unchanged**: `storage_row_read` per scanned input row; the window arguments' and
  partition/order keys' `operator_eval`s per input row; `row_produced` per emitted row; the
  projection's `operator_eval`s per emitted row.
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
- **The decimal ratios** (`percent_rank`/`cume_dist`) flow through `select_div_scale` + half-away
  rounding ([decimal.md](decimal.md) §4) — pinned with exact rendered strings, the highest
  cross-core risk after `AVG`.
- **Aggregate-window reuse** inherits the aggregate determinism contract unchanged (the
  order-independent float fold, the widening overflow boundaries).

## 10. Divergence ledger (CLAUDE.md §1/§8 — recorded, oracle-overridden)

Deliberate divergences from PostgreSQL, each registered in
[../conformance/oracle_overrides.toml](../conformance/oracle_overrides.toml):

- **D1 — within-partition order is pinned, not unspecified.** Absent a window `ORDER BY`, jed
  orders a partition by primary-key/scan order (§3); PG leaves it unspecified. jed is deliberately
  stricter on determinism (the §8 no-iteration-leak rule). Observable only for functions sensitive
  to peer/row sequence with no `ORDER BY` (e.g. `row_number() OVER (PARTITION BY g)`).
- **D2 — `percent_rank`/`cume_dist` → `decimal`, not `float8`.** jed keeps binary floats out of
  the value/output path (§8) and reuses the exact decimal division (the `AVG`→`decimal`/`SUM`→
  `decimal` family). Rendered as exact decimals; the oracle override records the type + rendering.
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
- **General-expression `PARTITION BY`/`ORDER BY`** (`PARTITION BY a + b`, and — in a grouped query
  (S8) — an aggregate *as* a window key, `ORDER BY sum(x)`) — window keys are column references only
  (the `sort_key` grammar), so an aggregate/expression key does not parse; lifted with the
  `GROUP BY`/`ORDER BY` expression-key follow-on (§1 S0 narrowing).
- **`RANGE` value offsets over a timestamp/timestamptz/date key** (an `interval`/integer offset, D4)
  — deferred; only integer/decimal ordering keys take a value offset this slice. A float key stays
  `0A000` permanently (D3). Non-literal/expression frame offsets are also out (literals only, like
  the `ROWS` narrowing).
- **`float8` results** for `percent_rank`/`cume_dist` (D2) — out unless a future need overrides.
- **The shared partition/sort pass** across distinct-but-compatible window definitions, and frame
  sliding-window optimizations — S5 and beyond (cost-lowering only, never correctness).
- **`IGNORE NULLS`** on `lag`/`lead`/`first_value`/… (SQL:2011, PG does not support it) — out.
