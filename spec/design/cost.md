# Cost accounting — design

> The reasoning behind the deterministic cost-accounting seam (CLAUDE.md §13). The
> canonical **data** is [../cost/schedule.toml](../cost/schedule.toml) (the unit
> weights); this doc is the *why* and — because cost is a cross-core contract with no
> reference implementation (§2) — the precise **accrual rules** every core must obey.
> The schedule is validated by [../cost/verify.rb](../cost/verify.rb) (`rake verify`).

A first-class use case is **safely evaluating untrusted, user-supplied queries**
(CLAUDE.md §13). That requires the engine to **deterministically meter the cost of
executing a query** and to **abort when a caller-supplied ceiling is reached**. Both halves
have landed: the metering **seam** — the cost counter threaded through the executor,
expression evaluator, and storage reads — and the **ceiling + deterministic abort** built on
it (§6). A caller sets `max_cost` on the handle (spec/design/api.md §8); the instant a
statement's accrued cost reaches it, execution aborts with `54P01` (`cost_limit_exceeded`).
A **second**, independent untrusted-query gate guards the *native call stack* rather than
accrued cost: a fixed maximum expression/query **nesting depth**, checked in the parser,
aborting with `54001` (`statement_too_complex`) before deeply-nested input can overflow the
stack — a hazard the cost ceiling structurally cannot catch (it strikes before metering). See
§7.

## 1. Why cost is a shared contract, not an implementation detail

Because there is no reference implementation (CLAUDE.md §2), the only thing that says two
cores agree is that they produce identical results on the same shared tests. Cost is no
different: the cost of a given `(query, database state)` is **fully deterministic** and
**byte-identical across every core**. This makes it a CLAUDE.md §8 divergence hotspot,
and so it is **asserted in the conformance corpus** (the `# cost:` directive —
[conformance.md](conformance.md)), not merely in per-core tests. A divergence in any
core's counting is a failing corpus entry the day it appears.

## 2. The unit schedule is data

The cost units and their weights live in [../cost/schedule.toml](../cost/schedule.toml)
(data over code, CLAUDE.md §5), emitted into each core as `@generated` constants by
[../../scripts/gen_costs.rb](../../scripts/gen_costs.rb) — the same codegen "middle path"
as the operator catalog ([codegen.md](codegen.md)). The accrual **sites** (which line in
the executor/evaluator/storage fires which unit) are hand-written per core; §5 forbids
codegenning the evaluator. Only the **weights** are shared data.

The core seam units, all weight `1`:

| unit | fires when |
|---|---|
| `storage_row_read` | one row is read from a table store during a scan |
| `page_read` | one B-tree node (page) is touched while scanning a store |
| `row_produced` | one row is emitted into a query result set |
| `operator_eval` | one interior expression node is evaluated |

(`page_read` was **added** in P6.3 when the store became a page-backed B-tree — §3
"`page_read`" — *alongside* `storage_row_read`, not a rename; the later
`aggregate_accumulate` unit, [../cost/schedule.toml](../cost/schedule.toml), is metered in
the aggregates path, the `value_compress`/`value_decompress` units — §3 "the compression
units" — in the large-value codec paths, the `decimal_work` unit — §3 "`decimal_work`"
— in the decimal arithmetic/comparison evaluations, the `gin_entry` unit — §3
"GIN-bounded scan" — in the GIN index gather, the `collate` unit — §3 "`collate`" —
in a collated comparison's sort-key build, and the `varlen_compare` unit — §3
"`varlen_compare`" — in a text/bytea comparison's byte scan.) Most weights are uniform on
purpose — phase 1 proved the seam reads cost from **data**; tuning the numbers later is a
data-only change touching no executor code. The per-operator `cost` field (functions.md §8) is
that hook made live: an operator charges its own base if catalog.toml authors one, else the
uniform `operator_eval` (§3).

## 3. Accrual rules (the cross-core determinism contract)

These rules are the contract. They must be implemented **identically** in Rust, Go, and
TS; any deviation diverges the count and fails the corpus.

- **`storage_row_read`** is charged once per row pulled from a store, at the top of the
  executor scan loop, **before** the filter runs — in `SELECT`, `DELETE`, and `UPDATE`.
  It is charged in the **executor loop, not inside the storage iterator**: the Rust store
  returns a lazy iterator while Go/TS materialize a sorted slice, so charging in storage
  would diverge the (future) abort *point*. The executor loop is the one place all three
  cores agree.
- **`page_read`** is charged once per B-tree node (page) in a table's store when that store
  is scanned, as a block **before** that table's `storage_row_read`s — the dedicated
  subsection below gives the rule (a full scan touches every node, so the charge is the
  tree's structural node count).
- **`row_produced`** is charged once per row that survives the filter and is projected
  into a `SELECT` result set, at projection time (post-filter, post-`ORDER BY`, **and
  post-`LIMIT`/`OFFSET`**). `LIMIT`/`OFFSET` slice the sorted rows *before* the projection
  loop, so a row skipped by `OFFSET` or excluded by `LIMIT` charges **no** `row_produced`
  or projection cost — only the windowed rows do. Whether such an excluded row still pays
  `storage_row_read` + its filter `operator_eval`s depends on the plan: with a blocking
  operator (an `ORDER BY`, `DISTINCT`, aggregate, or join) the scan must read every row
  first, so it does; without one, the **LIMIT short-circuit** (subsection below) stops the
  scan once the window is filled, so it does **not**. `DELETE` / `UPDATE` emit no rows and so
  charge no `row_produced`.
- **`operator_eval`** is charged once per **interior** expression node — `cast`, `neg`,
  `not`, `arith`, `compare`, `and`, `or`, `is_null`, `distinct`. **Leaf nodes — `column`
  and the constants (`int`/`bool`/`null`) — charge nothing.** Charging leaves would make
  cost track how many literals the parser happened to fold, an accidental property; cost
  must track genuine evaluation work. A **decimal** arithmetic/comparison node additionally
  charges size-scaled `decimal_work` — the dedicated subsection below.
- **No short-circuit.** Both operands of every binary node (`and`, `or`, `compare`,
  `arith`, `distinct`) are **always** evaluated before the node charges its own
  `operator_eval`. This is already true — the Kleene helpers (`and3`/`or3`/`boolAnd`)
  are pure functions over already-computed operand values, never control flow. The seam
  **must not introduce** a short-circuit: skipping the RHS in one core when the LHS is
  FALSE/NULL would drop that core's operand evals and diverge the count.
- **`CASE` is the one deliberate exception to no-short-circuit.** A `CASE`
  ([grammar.md](grammar.md) §23) charges its own `operator_eval` for the node, then evaluates
  its `WHEN` conditions **in source order, stopping at the first one that is TRUE** — a FALSE
  or NULL/UNKNOWN condition falls through. Only the conditions tested **up to and including the
  match**, plus the **selected** result (the matching `THEN`, or the `ELSE`, or nothing for an
  implicit `ELSE NULL`), are evaluated and charged; later arms are **not** evaluated. This
  short-circuit is *required* by PostgreSQL semantics — `CASE WHEN a = 0 THEN 0 ELSE 1 / a END`
  must not divide by zero on the `a = 0` rows — so it is a sanctioned exception, not a seam
  violation. It stays deterministic per `(query, db state)` because the evaluation order (first
  match wins, conditions left to right) is fixed across cores, so which arms accrue is itself
  deterministic. (A consequence, like `DISTINCT`'s, is observable: `CASE WHEN true THEN 0 ELSE
  1/0 END` succeeds and costs *less* than the eager form would, because the `1/0` arm is never
  reached. The simple form `CASE x WHEN v …` desugars each branch to `x = v`, so the operand is
  evaluated once per tested branch — the same per-branch model as `IN`'s LHS.)
- **Pre-order, LHS-before-RHS.** A node charges itself, then evaluates its left operand,
  then its right. The order does not change the **total** (a sum is order-independent),
  but it fixes the deterministic **abort point** for the cost ceiling (§6) identically
  across cores.
- **Helpers are not separately charged.** `eval_arith`/`evalArith`, and the `<=`/`>=`
  comparisons' internal `lt3 OR eq3` combinators, are covered by their owning node's
  single `operator_eval`. They are not `RExpr` nodes.

### `page_read` — the pages a scan touches

The store is a **page-backed copy-on-write B-tree** (P6.1, [storage.md](storage.md) §6): each
table's rows live in a tree of fixed-size pages, and the node boundaries are a §8 byte contract
— *the same tree shape in every core* (the in-memory B-tree **is** the on-disk one, node-for-page).
So the number of pages a scan touches is **deterministic and byte-identical across cores**,
exactly the property cost requires.

- **A full table scan walks the whole tree**, so it charges `page_read` once per **node**
  (interior *and* leaf) in that table's tree — its structural **node count**. A scan with no
  usable primary-key bound (a predicate on a non-key column, an OR, or no WHERE) reads the entire
  store (the same loop that charges `storage_row_read` per row), so it touches every page. An
  **empty table** (no root) has zero nodes and charges no `page_read`. A scan whose WHERE bounds
  the primary key touches **fewer** pages — see "bounded scan / point lookup" below.
- **`page_read` is charged as a block, before that table's `storage_row_read`s** — read the
  pages, then the rows within them. Charged at the **same three sites** as `storage_row_read`
  (the `SELECT`/JOIN materialization, the `DELETE` scan, the `UPDATE` phase-1 scan), once per
  table-scan *execution*. The total is order-independent, but fixing the block-before-rows order
  pins the cost-ceiling abort point (§6) identically across cores.
- **It composes exactly like `storage_row_read`.** A **JOIN** materializes each base table
  once, so it charges each table's node count once (Σ over the relations — a self-join counts
  the table twice, once per alias). A **set operation** charges each operand's scans
  (`lhs + rhs`). An **uncorrelated** subquery (folded once) charges its tree once; a
  **correlated** subquery re-scans its inner table **per outer row**, charging that node count
  each time — identical to how those forms already compose `storage_row_read`.
- **Logical, not physical.** `page_read` counts the tree's structural node count — a *logical*
  page access — **not** a physical disk fetch. A future buffer pool / demand-paging cache for
  larger-than-RAM files (CLAUDE.md §9) serves a page from memory or disk transparently; the
  cost is identical either way, so the deterministic cost stays cache-independent (§13).
- **Overflow chains count too — for the columns the query references**
  ([large-values.md](large-values.md) §8.1/§12/§14). A record whose values spilled out-of-line
  stores them on a chain of overflow pages (`page_type 4`), and materializing such a value reads
  its chain — so the scan's `page_read` block also counts **one per overflow chain page of every
  record the scan's bound admits, for every spilled column in the query's *touched set*** (the
  full table when unbounded; the in-range records for a bounded scan, so a point lookup pays only
  *its* record's chains and a miss pays none — and a query that never references the spilled
  column pays nothing for it, however many records it admits). The touched set is **static**
  (below), so the charge stays an up-front block that does **not** short-circuit under `LIMIT`
  (see "LIMIT short-circuit"; tightening to the rows actually emitted is a possible later
  refinement, exactly like the leaves-actually-faulted note there). The chain page count is
  `ceil(stored / C)` per externalized value, where `stored` is the bytes the chain actually
  carries — the content payload for an external-plain value, the **compressed** block for an
  external-compressed one (large-values.md §13) — a function of the §8-contracted disposition
  rule and chain layout, so it is byte-identical across cores; a fully-inline table charges
  exactly the structural node count as before (existing costs do not move). The charge stays
  *logical* (§13): it models the lazy read-on-touch executor (large-values.md §7/§14) whether or
  not the engine physically reads eagerly today, the same way `page_read` predates the buffer
  pool.

### The touched set — which columns a scan "reads"

The **touched set** of a relation is the set of its columns the query **statically references**,
collected at plan time from the resolved expression trees (a §8 contract — every core collects
identically): the WHERE filter, every JOIN `ON`, and — for a non-aggregate query — the
projections and `ORDER BY` keys, or — for an aggregate query — the `GROUP BY` keys and every
aggregate's argument (an aggregate query's projections / `HAVING` / `ORDER BY` reference the
synthetic group row, whose inputs those keys and arguments already are). A **correlated
subquery's outer reference** into the relation counts (collected depth-aware through nested
plans). The set is per `(query, relation)` and purely syntactic: a column referenced only in a
never-taken `CASE` branch is still touched. Consequences worth naming:

- `SELECT small_col FROM t WHERE pk = $1` touches neither a spilled `body` column's chain nor a
  compressed value's slabs — the large-values headline case (large-values.md §7).
- `SELECT count(*) FROM t` and `EXISTS (SELECT 1 FROM t …)` touch **no** columns of `t`: they
  charge the structural node block and row reads only.
- **`DELETE`** touches only its filter's columns — dropping a row never reads its chains.
- **`UPDATE`** touches its filter's columns plus every assignment **source**'s columns. The
  rewrite itself does not *read* an untouched stored value (under the §14 model an unchanged
  spilled value's bytes move without decompression); the write side stays metered by
  `value_compress` per stored row version, unchanged.

### `value_compress` / `value_decompress` — the compression units

Transparent LZ4 compression (large-values.md Slice B, [../fileformat/lz4.md](../fileformat/lz4.md))
is real CPU work in both directions, metered by two units so the §6 ceiling can bound it. Both are
quantized in **`C`-byte slabs of the *decompressed* (raw) payload** — `ceil(raw_len / C)` with
`C = page_size − 16`, the same slab size the overflow chains use — proportional to the work yet
computable from the stored lengths alone, so the charge never requires re-running the codec.

- **`value_decompress`** joins the scan's **up-front block** next to the chain `page_read`s: for
  every record the scan's bound admits, each **compressed** stored value (inline-compressed `0x03`
  or external-compressed `0x04`) **in a touched column** (the touched set above) charges
  `ceil(raw_len / C)`. The same composition rules apply verbatim — per JOIN base table, per
  correlated re-scan, no `LIMIT` short-circuit, nothing for a missed bound, nothing for an
  untouched column, and a table with no compressed value charges nothing.
- **`value_compress`** is the write side: an `INSERT`/`UPDATE` whose record exceeds `RECORD_MAX`
  runs the disposition decision's compress pass, and **every attempt** (adopted or rejected by
  *store-smaller* — the encoder ran either way) charges `ceil(raw_len / C)`. Charged once per
  stored row version at the statement's write site, never for the B-tree's internal re-encodes.
  A record that fits inline-plain attempts nothing, so existing costs do not move.

### `decimal_work` — size-scaled decimal arithmetic

A decimal value can now reach PostgreSQL's format caps — 131072 integer + 16383 fractional
digits ([decimal.md](decimal.md) §2) — so a single multiplication can be ~10⁹ limb operations.
A flat `operator_eval` would let an untrusted query buy that CPU for one unit (§1, CLAUDE.md
§13), so decimal arithmetic and comparison evaluations charge an **additional**
`decimal_work × (W − 1)`, where **W is the operation's work in base-10⁴ digit groups** — the
on-disk digit unit ([format.md](../fileformat/format.md)), deliberately **not** a core's
internal limb base (Rust/Go use base-10⁹, TS base-10⁴; the group count is computed from the
logical digit counts, identical everywhere).

Definitions, with each operand taken **after** `int → decimal` promotion: `d` = significant
digits of the coefficient (`0` for zero), `s` = display scale, and `q(n) = max(1, ceil(n/4))`.
For the scale-aligning operations let `s* = max(s1, s2)` and `aᵢ = dᵢ + (s* − sᵢ)` (the digit
count after the lower-scale coefficient is multiplied up). Then:

| operation | W |
|---|---|
| compare (`=` `<>` `<` `<=` `>` `>=`, `IS [NOT] DISTINCT FROM`, one `IN`-list element) | `max(q(a1), q(a2))` |
| `+` `−` | `max(q(a1), q(a2))` |
| `*` | `q(d1) · q(d2)` |
| `/` | `q(d1 + E) · q(d2)` with `E = rscale + s2 − s1` (`rscale` per `select_div_scale`, decimal.md §4) |
| `%` | `q(a1) · q(a2)` |
| `SUM`/`AVG` fold | the `+` formula, accumulator vs. input |

The rules:

- **Charged before the work runs, and immediately guarded.** The W − 1 units accrue *before*
  the limb loop executes, and the charge is **immediately followed by a §6 ceiling guard** —
  a new enforcement point alongside the per-node/per-row/per-fold guards — so a ceiling
  aborts ahead of the expensive operation, not after it. The abort point stays deterministic
  (the charge+guard is a single block at the owning node, after the node's `operator_eval`
  and its operand evaluations, mirrored identically across cores).
- **W − 1, not W.** The first group rides the operation's flat `operator_eval`. Operands of
  ≤ 4 aligned digits — every int-promoted small constant, money, ordinary literals — have
  W = 1 and charge **nothing**, so costs predating this unit are unchanged.
- **A NULL operand charges nothing** (the operation short-circuits to NULL before any limb
  work — same as its result rule). A **zero divisor/modulus charges nothing** and traps
  `22012` (the trap precedes the work). A zero **dividend** still charges by the formula
  (uniform, no special case; `d = 0` keeps it small).
- **Comparison nodes charge once**, even where a core's `<=`/`>=` decomposes into
  `lt3 OR eq3` internally (the "helpers are not separately charged" rule above).
- **Aggregate folds**: each `SUM`/`AVG` accumulate over decimals charges the `+` formula
  against the running accumulator (deterministic — rows fold in scan order, which is key
  order). `MIN`/`MAX` folds are direct `Value` compares like the sort's, and stay unmetered
  (the boundary below).
- **Linear single-pass work stays flat**: unary `−` / `abs`, casts and `round`/typmod
  rescale, literal parse, rendering, and the key-canonicalization in GROUP BY/DISTINCT are
  all O(digits) one-pass over a value the scan already paid for (`page_read` chains +
  `value_decompress`), with no quadratic blow-up — they keep their single flat charge (or
  none, where they were already unmetered). The quadratic operations are the attack
  surface; they are what scales.

### `regex_compile` / `regex_step` — regular-expression compile and match

Regular expressions ([regex.md](regex.md)) are a hand-written **Pike-VM** NFA simulation — the
RE2-style design chosen precisely so matching is **linear in the input with no backtracking**, an
untrusted-query safety property that holds *independent of* the cost meter (§13). But the work is
still real and an attacker can still drive it (a large pattern, a long subject, a deeply-unrolled
`{n,m}`), so it is metered by two units, both accrued like `decimal_work` (charge, then guard):

- **`regex_compile`** fires once per NFA instruction **emitted** while compiling a pattern, so the
  charge equals the program length `|program|`. `{n,m}` is **unrolled** at compile (regex.md §3.3),
  so a quadratic-expansion pattern (`(a{1000}){1000}`) accrues `regex_compile` proportional to the
  expansion and a ceiling aborts it. A **constant** pattern (the `col ~ 'literal'` case) compiles
  **once** — charged `|program|` units at statement-execution start, *not* per row (the
  precompilation contract, regex.md §5); a per-row pattern charges `|program|` per compile.
- **`regex_step`** fires once per Pike-VM **thread-step** — each instruction dispatched in the main
  consume loop or the epsilon-closure. The per-position **dedup-by-pc** bounds the threads at any
  input position to `≤ |program|`, so total `regex_step ≤ |program| × (|input| + 1)` — linear, the
  RE2 bound. Guarded once per input position, so a runaway match aborts `54P01` deterministically.

The `~`/`~*`/`!~`/`!~*` node charges **one** `operator_eval` for the whole match (the LIKE
precedent), and `regex_step` on top — which is the per-step work LIKE's matcher leaves unmetered
(LIKE is linear in the input *alone*; regex in program × input, so it needs the explicit unit). A
**well-formed but too-large** program — one exceeding `MAX_REGEX_PROGRAM` (32768 instructions) — is
the third structural-complexity trigger of `54001` (§7/§7b), checked *projectively* at compile so
the *unlimited* handle (`max_cost = 0`) is protected where the ceiling cannot reach. The two units'
counts are pinned cross-core by `spec/regex/{program,match}_vectors.toml` and the `# cost:` corpus
directives, so the accrued cost and abort points are identical in every core (§8/§13).

### Bounded scan / point lookup — the pages a primary-key predicate touches

A **single-table** WHERE on the **primary key** does not need the whole tree. Because the key
encoding is **order-preserving** ([encoding.md](encoding.md) — raw byte order *is* value order),
a primary-key comparison maps to a contiguous range of storage keys, and the scan visits only the
B-tree nodes that range can intersect. This is the engine's first index-style access path; it is a
deliberate, cost-visible optimization, gated by the `query.point_lookup` capability.

- **Which predicates bound.** Flatten the WHERE's top-level **AND-chain** (an `OR` is never
  descended — a disjunction is not one contiguous range) and collect every conjunct of the form
  `pk <cmp> const-source` (`=`, `<`, `<=`, `>`, `>=`; the primary key on either side; `BETWEEN`
  desugars to `pk >= a AND pk <= b`, so it falls out for free). The `const-source` must be of the
  **primary key's own type** — a promoted comparison (e.g. `intpk = 2.5`) does **not** bound — and
  is one of: a **literal**, a **bind parameter** `$N`, or (the correlated case below) a bare
  **enclosing-query column**. Every other conjunct stays in the **residual filter** (the whole
  WHERE, re-applied to each scanned row), so the bound is always a *superset* of the matching rows
  and the result is unchanged. A no-PK relation is **not** bounded (it keeps the full-scan cost
  above). In a **JOIN** each base table is bounded *independently* by the WHERE conjuncts on **its
  own** primary key against such a const-source (`query.join_pushdown`, "/ JOIN" below); a
  cross-relation `b.pk = a.x` is **not** bounded (a follow-on — see "/ JOIN").
- **`page_read` = the nodes the bound's key range intersects.** A scan visits the root, then
  descends only into a child subtree whose separator span can overlap the range — so a **point
  lookup** (`pk = c`) charges the root→leaf path (the tree height), and a **range** charges the
  path plus the contiguous run of leaves the range spans. The unbounded range (`−∞..+∞`, the full
  scan) intersects every node, so it reduces to the node count above — **existing full-scan costs
  do not move.** The overlap is computed from the resident interior separators **without faulting**
  a leaf, so it stays a *logical* count (the buffer-pool-invisible property holds). It is
  byte-identical across cores because the tree shape and the descent rule are both a §8 contract.
- **`storage_row_read` = the rows in range.** Only the rows whose key lies within the bound are
  read and charged (and then filtered) — a point lookup reads 0 or 1 row, not the whole table.
  The residual filter's `operator_eval`s therefore accrue only over the in-range rows.
- **A provably empty range charges nothing.** A `pk = NULL` (3VL-unknown) or contradictory bounds
  (`pk > 5 AND pk < 5`) admit no key, so the scan reads no page and no row — `page_read` 0,
  `storage_row_read` 0, and a mutation deletes/updates nothing. (A point-lookup *miss* on an
  existing key range — `pk = 99` where 99 isn't stored — still visits the leaf it would live in,
  so it charges that path's `page_read` but reads no row.)

`spec/conformance/suites/query/point_lookup.test` pins these costs cross-core; the bounded forms in
`expr/cost.test`, `query/distinct.test`, `query/limit_offset.test`, and `query/select_list.test`
exercise them in context.

### Index-bounded scan — a secondary index narrows a base-relation scan

A **secondary index** ([indexes.md](indexes.md)) gives a second bound kind at the same
per-relation pushdown seam. For each base relation of a **SELECT** scan (single-table, a JOIN
base table, or a correlated subquery's inner table), the plan picks the **single-column PK
bound first** (it is the row's own key — no second tree, range-capable, strictly cheaper);
else, among the relation's indexes whose **first key column** has at least one **equality**
conjunct `col = const-source` in the WHERE AND-chain (the same const-source rule as above —
literal / `$N` / correlated outer column, type-matched), the index with the **lowest
lowercased name**; else the full scan. Gated by the `ddl.secondary_index` capability, pinned
cross-core in `spec/conformance/suites/query/index_scan.test`.

The index-bounded scan accrues, in place of the full-scan block:

- **`page_read` × the index-tree nodes** overlapping the equality prefix range (the same
  overlap rule as the PK bound, applied to the index tree — a logical count, never faulted).
- **Per admitted entry, the row fetch**: `page_read` × the **table-tree** nodes overlapping
  the *point* bound of that entry's row storage key (the root→row descent), plus that row's
  touched-column `value_decompress` slabs — i.e. each row fetch costs exactly what a PK point
  lookup of that row costs.
- **`storage_row_read` per fetched row**, and the residual filter / projection /
  `row_produced` unchanged. The WHERE stays the residual filter; the bound only narrows
  which rows are fetched.
- **A provably empty bound charges nothing**: an equality against NULL (3VL), contradictory
  equalities (`a = 1 AND a = 2`), or an out-of-range integer admit no entry — no page, no row.

Deterministic and byte-identical across cores: the index tree shape, the entry-key encoding,
and the overlap rule are all §8 contracts. **Narrowings this slice** (indexes.md §5): first
key column only, equality only, SELECT scans only (UPDATE/DELETE keep their PK pushdown), and
**no LIMIT-streaming combination** — an index-bounded scan with a LIMIT takes the eager path
(reads the full admitted set; the short-circuit below stays PK/full-scan-only).

**DDL costs.** `CREATE INDEX` charges its build scan over the existing rows: `page_read` ×
the **table's** full node count + `storage_row_read` per row (the build's touched set — the
indexed columns — is fixed-width, so its chain/decompress terms are structurally zero); an
empty table charges 0. `CREATE UNIQUE INDEX` charges **exactly the same** — its duplicate
verification (indexes.md §8) is unmetered validation, like the uniqueness probes below.
`DROP INDEX` charges 0 (a pure catalog edit, like DROP TABLE). Index **maintenance** at
INSERT/UPDATE/DELETE is unmetered ("What is NOT metered" below).

### GIN-bounded scan — an inverted index narrows an array-column scan

A **GIN index** ([gin.md](gin.md)) gives a third bound kind at the same per-relation seam (after
the PK bound and the ordered-index equality bound). For a base relation of a **`SELECT`, `UPDATE`,
or `DELETE`** scan whose WHERE has a conjunct `col @> Q` (contains), `col && Q` (overlaps),
`c = ANY(col)` (membership), or `col = Q` (exact array equality) where `col` is GIN-indexed and the
query operand is a **constant**, the scan gathers candidates from the index instead of
full-scanning. Gated by the `query.gin_scan` capability (with `query.gin_any_eq` for `= ANY`,
`query.gin_array_eq` for array `=`, and `query.gin_mutation` for the `UPDATE`/`DELETE` bound), pinned
cross-core in `spec/conformance/suites/query/gin_scan.test` (and `gin_any_eq.test` /
`gin_array_eq.test` / `gin_mutation.test`).

A GIN-bounded scan accrues, in place of the full-scan block:

- **`page_read` × the entry-tree nodes** overlapping each query term's prefix range — the same
  overlap-node rule as the ordered index, applied **once per query term** (`@>` and array `=`
  gather all of `Q`'s distinct non-NULL elements, `&&` its distinct non-NULL elements, `= ANY` the
  single scalar term — gin.md §2).
- **`gin_entry` × the posting entries visited** across all term scans — the per-entry combine work
  (intersection for `@>` / array `=` / `= ANY`, union for `&&`) that the per-node `page_read`
  under-meters when a posting list is long.
- **Per candidate row** (post-combine, point-looked-up in storage-key order): `page_read` × the
  table-tree nodes on its descent + its touched-column `value_decompress` slabs +
  **`storage_row_read`** — each candidate fetch costs exactly a PK point lookup of that row.
- **The residual filter** — the original `@>` / `&&` / `= ANY` / `=` predicate stays the residual
  WHERE filter, so one `operator_eval` per candidate — and **`row_produced`**, unchanged.

A **provably-empty** bound charges nothing — a NULL `Q` (or array-`=` against NULL), an `@>` whose
`Q` holds a NULL element (never TRUE under strict equality), an `&&` whose `Q` has no non-NULL
element, or a NULL `= ANY` scalar. Two **full-scan fallbacks** charge the full scan (rows the index
cannot enumerate, having no terms): `@> '{}'` (every non-NULL array contains the empty array), and
array `=` whose `Q` has no non-NULL element (`col = '{}'` / `col = ARRAY[NULL,…]`). Deterministic and
byte-identical across cores: the term extraction, the term encoding, the entry-tree shape, and the
overlap rule are all §8 contracts (gin.md §8). **Narrowings this slice** (gin.md §6): constant query
operand only, `@>`/`&&`/`= ANY`/`=` only, and no LIMIT-streaming combination. A **GIN-bounded
`UPDATE`/`DELETE`** accrues this same scan block in place of its full-scan block (its target-row scan
uses the **PK then GIN** bound — not the ordered-index bound, which stays SELECT-only); so a
`DELETE … WHERE col @> Q` costs the matching `SELECT`'s scan minus the `row_produced` a bare mutation
omits (a `RETURNING` clause restores it plus its projection units), and the phase-2 rewrite/remove +
index maintenance are unmetered writes ("What is NOT metered" below).
**DDL cost:** `CREATE INDEX … USING gin` charges its
build scan — `page_read` × the table's node count + `storage_row_read` per row, plus the array
column's overflow-chain `page_read` / `value_decompress` if its values spilled (the build's touched
set is the array column, which **can** be large — unlike the fixed-width ordered build); an empty
table charges 0, `DROP INDEX` charges 0.

### `collate` — a non-`C` collation's per-code-point sort-key work

A non-`C` collation ([collation.md](collation.md)) orders text by its **UCA sort key** rather than
raw bytes; building that key is per-code-point work the byte comparator does not do. The **`collate`**
unit (weight 1) is charged at the **comparison-operator evaluation** site — the deterministic,
cross-core-identical metering point:

- A collated **ORDERING** comparison (`< <= > >=`, i.e. `RExpr::Compare` whose derived collation is a
  loaded non-`C` table) over two **non-NULL text** operands charges `collate × (codepoints(lhs) +
  codepoints(rhs))` — code points, **not** UTF-16 units or bytes (the cross-core count, collation.md
  §6 / CLAUDE.md §8) — *in addition to* the node's one `operator_eval`. The charge is guarded
  immediately, so a cost ceiling aborts a runaway collated comparison (54P01).
- **`=`/`<>` charge no `collate`** even under a collation: deterministic-collation equality **is**
  byte-identity (collation.md §7), so they take the plain `eq3` path. A `C` / default comparison
  (collation `None`) charges nothing here either. (Both of those paths instead charge `varlen_compare`
  for the byte/code-point scan — the subsection below; `collate` and `varlen_compare` partition the
  text/bytea comparison surface, never both on one node.)
- A **NULL** operand charges no `collate` (the comparison is Unknown before any sort key is built).
- The **`ORDER BY` sort is unmetered**, like every sort ("What is NOT metered" below, spill.md §6):
  a collated `ORDER BY` materializes its survivors and sorts them with a **decorate** sorter that
  builds each row's sort key **exactly once** (no `O(n log n)` recompute), but charges no `collate` —
  its input cardinality is already bounded by the upstream `storage_row_read` / `row_produced`. (The
  set-operation sort path carries no `Meter` at all, so the comparison evaluator is the one
  consistent, meterable site — collation.md §11.) Pinned cross-core by `# cost:` assertions in
  `spec/conformance/suites/collation/collate.test`.

### `varlen_compare` — a text / bytea comparison's length-scaled byte scan

A comparison node charges **one** `operator_eval` regardless of operand size, but comparing two long
`text` or `bytea` values is **O(length)** work — and an untrusted query can multiply that length work
by **fan-out**: a join or correlated re-scan runs the same comparison `|A| × |B|` times, while the
scan's one-time `value_decompress` / `page_read` paid for each value only **once**. So a flat charge
lets `SELECT … FROM big a JOIN big b ON a.t = b.t` do length × N² comparison work for an N²-flat cost.
The **`varlen_compare`** unit (weight 1) closes that gap — the text/bytea analog of `decimal_work`,
charged at the **comparison-operator evaluation** site (the deterministic, cross-core-identical
metering point, like `collate`):

- A comparison (`= <> < <= > >=`) over two **non-NULL** `text` or two **non-NULL** `bytea` operands
  charges `varlen_compare × (W − 1)`, where **W = max(1, min(len(lhs), len(rhs)))** — the **shorter**
  operand's length, counted in **code points** for `text` (the cross-core count, **not** UTF-16 units
  or UTF-8 bytes — CLAUDE.md §8) and in **bytes** for `bytea` — *in addition to* the node's one
  `operator_eval`. The charge is guarded immediately, so a cost ceiling aborts a runaway comparison
  (54P01) **before** the byte scan.
- **Why `min`, not the `max` decimal-compare uses.** A byte / code-point comparison stops at the first
  differing position or the end of the **shorter** operand (equality of unequal lengths is O(1); an
  ordering compare never reads past the shorter string), so `min` is a true upper bound on the work.
  Equally important, `min` keeps a legitimate `WHERE body = $short` against one large-`text` row
  **cheap** (a length mismatch costs ~1), which a `max`-based charge would wrongly inflate to the big
  value's length. Decimal compare uses `max` because magnitude comparison has no length short-circuit;
  the divergence is principled, not an inconsistency.
- **W − 1, like `decimal_work`.** The first unit rides the flat `operator_eval`; operands of ≤ 1 unit
  (one code point / one byte, or an empty string) have W = 1 and charge **nothing**, so small-string
  costs predating this unit are unchanged.
- **It complements `collate`, never doubles with it.** The collated **ORDERING** path (a non-`C`
  collation, `< <= > >=`) charges `collate` and returns *before* this charge, so `varlen_compare`
  covers exactly the rest of the text/bytea comparison surface: **`=`/`<>`** (byte-identity under any
  collation), **`C` / default-collation ordering**, and **all `bytea`** comparison (bytea has no
  collation). The two units partition the surface.
- A **NULL** operand, or a non-`text`/`bytea` pair (integers, decimals — `decimal_work` handles those),
  charges no `varlen_compare`.
- The **`ORDER BY` sort stays unmetered** like every sort (the `collate` rule above) — the unit lives
  at the expression-evaluation site, not the sort. Pinned cross-core by `spec/conformance/suites/expr/varlen_compare.test`
  (and the join `# cost:` baselines in `joins/inner.test` / `joins/left.test`), with the ceiling-abort
  amplification in `resource/dos_amplification.test`.

**Bounded scan / JOIN — each base table bounded by its own PK predicate.** In a multi-table FROM
each base table is materialized independently (see "JOIN" below), so each is bounded **on its own**
by the WHERE conjuncts on **its** primary key against a const-source — exactly the per-relation form
of the rule above. `SELECT … FROM a JOIN b … WHERE a.pk = 5` materializes a's matching row (a seek)
and full-scans b; `WHERE a.pk = 5 AND b.pk = 10` seeks both. A point-lookup **miss** on a join key
(`a.pk = 999`) materializes **zero** rows for that table, so the nested loop has nothing to drive —
the join collapses to the other tables' scan cost. The bound's source is still a constant
(literal/param/outer); a **cross-relation** `b.pk = a.x` is **not** bounded — binding b's key to a's
value per outer row is the index-nested-loop case, a follow-on (a sibling column is not a
const-source). Bounds come only from the **WHERE**, never an `ON` (an ON failure NULL-extends rather
than drops, so it is not a post-join filter). **Sound for outer joins:** a non-NULL PK conjunct in
WHERE is unknown for a NULL-extended row, so it discards every NULL-extension of that relation — the
LEFT/RIGHT/FULL join degenerates to INNER on the bounded side, and any surviving output row has that
PK in range, so bounding the table cannot drop it. Gated by `query.join_pushdown`, pinned cross-core
in `spec/conformance/suites/joins/pushdown.test`.

**Bounded scan / correlated — the inner PK bound from an outer column.** A correlated subquery is
re-executed once per outer row (see "Subqueries" below). When its inner query is a **single table**
whose WHERE compares the inner **primary key** to an **enclosing-query column** — `inner.pk = o.col`
(or `<`, `<=`, `>`, `>=`) — the bound's `const-source` is that outer column, resolved to **the
current outer row's value** each time the inner runs. So the inner **seeks** (a per-outer-row point
lookup/range) instead of re-scanning the whole inner table for every outer row: across N outer rows
the inner's `storage_row_read` drops from `N × |inner|` to `N ×` (rows in range, 0/1 for a point
lookup), and `page_read` from `N × node_count` to `N ×` the access-path nodes. It is the **same
bounded-scan mechanism** — the only addition is that the source is read from the outer row rather
than a literal/param — so soundness is identical (the whole WHERE stays the residual filter) and the
**rows are unchanged**; only the inner re-scan cost drops. The bound is still fully deterministic per
`(query, db)`: the outer rows are deterministic, so each per-outer-row bound — and its cost — is too,
and it is byte-identical across cores (the outer value, the key codec, and the overlap rule are all
shared). A NULL outer value gives a 3VL-empty bound (the inner reads nothing). This is gated by the
`query.correlated_pushdown` capability and pinned cross-core in
`spec/conformance/suites/subquery/correlated_pushdown.test`. JOIN base tables and no-PK inners stay
unbounded (the same follow-on as above).

### LIMIT short-circuit — stopping the scan when the window is filled

A `LIMIT` normally windows *after* the scan, so every scanned row pays `storage_row_read` even when
the window excludes it (above). But when the query is a **single table with no blocking operator** —
no join, aggregate, `DISTINCT`, or `ORDER BY` — there is nothing that needs to see all the rows, so
the engine **streams** scan→filter→project and **stops the scan the instant the `LIMIT`/`OFFSET`
window is filled.** This is the engine's first early-out, gated by the `query.limit_short_circuit`
capability.

- **`storage_row_read` counts only the rows actually read.** The scan reads in primary-key order,
  skipping `OFFSET` passing rows and producing `LIMIT` rows, then **stops** — so it charges
  `storage_row_read` (and the filter's `operator_eval`s) only for the rows up to that point, not the
  whole table. `SELECT v FROM u LIMIT 2` over a 5-row table reads 2 rows, not 5. This is the
  deliberate cost change; it is genuine (the scan really stops — leaves past the stop point are never
  faulted), not a post-hoc truncation, so the cost honestly bounds the work (CLAUDE.md §13).
- **`page_read` does NOT short-circuit** — it stays the full block (the scan bound's node count
  plus the bound's overflow chain pages and `value_decompress` slabs, charged up front), so a
  `LIMIT` does not lower it. Keeping
  `page_read` the structural count preserves its "logical, buffer-pool-invisible" definition and one
  accrual model across all scans; the row reads are where the early-out shows. (Tightening
  `page_read` to the leaves actually faulted is a possible later refinement; it would only matter
  for a very large multi-leaf table.)
- **A *blocking* `ORDER BY` (or any blocking operator) keeps the full scan.** A sort the scan does
  **not** already satisfy — a non-PK key, or `DESC` (a reverse scan is a follow-on) — must
  materialize every row before windowing, so it charges `storage_row_read` for all of them (the rule
  at the top of this section). But an `ORDER BY` the scan **does** satisfy is *not* blocking: see
  "ORDER BY satisfied by primary-key order" below, which short-circuits exactly like the no-`ORDER
  BY` case. `query/limit_offset.test` pins both sides — `ORDER BY id` (the PK) short-circuits while
  `ORDER BY val` (a non-PK) scans all rows.
- **Composes with the PK bound.** A `WHERE pk <range> ... LIMIT n` first bounds the scan to the key
  range (above), then short-circuits within it once `n` rows are produced. (An **index** bound does
  **not** stream — an index-bounded scan with a LIMIT takes the eager path; see the index-bounded
  scan subsection above.)
- **The rows are identical** to the eager path: the `offset..offset+limit` slice of the
  primary-key-ordered filtered rows. (The *result set* of a `LIMIT` with no `ORDER BY` is
  SQL-unspecified — CLAUDE.md §8 — but our cores agree, scanning in primary-key order.)

`query/limit_offset.test` pins these costs cross-core (a uniform-value table makes the no-`ORDER BY`
subset deterministic so a specific result can be asserted alongside the `# cost:`).

### ORDER BY satisfied by primary-key order — eliding the sort

An `ORDER BY` is normally a **blocking** operator: the engine must read every row, sort, then window
(the rule at the top of this section). But when the requested order is *already* the order the scan
produces, the sort is a no-op and is **elided** — the scan streams rows straight to the window, and
(with a `LIMIT`) short-circuits exactly like the no-`ORDER BY` case above. Gated by the
`query.order_by_pk_scan` capability.

The base-table scan walks the table tree forward in **storage-key (primary-key) order**, so an
`ORDER BY` is satisfied by the scan when it is a single-table, non-aggregate, non-`DISTINCT` `SELECT`
whose `ORDER BY` keys are a **prefix of the PRIMARY KEY columns** (in key order), each:

- **`ASC`** — the forward scan is ascending; `DESC` (a reverse traversal) is a follow-on, and keeps
  the blocking sort.
- sorting by the **same order the stored key realizes** — for a collated key, the column's frozen
  collation (the tree stores the UCA sort key, so its byte order *is* the collation order —
  [encoding.md §2.12](encoding.md), [collation.md §8](collation.md)); a mismatching explicit
  `COLLATE` keeps the blocking sort. A **version-skewed** collated key (collation.md §12) is never
  used for order — the stored keys are at the file's pinned version, so the scan order would be wrong
  for the loaded one; it keeps the blocking sort, which recomputes against the loaded collation.

The PK columns are `NOT NULL`, so a key's `NULLS FIRST|LAST` is a no-op (no NULLs to place). Two
coverage shapes both qualify: an `ORDER BY` **shorter** than the PK is a prefix — ties are broken by
the remaining PK columns, which is exactly the canonical PK tie-break the eager stable sort produces;
an `ORDER BY` that runs **past** the full PK matches the whole (unique) key, so its extra keys are
redundant (no ties remain).

- **Cost — no `LIMIT`.** The scan reads every row either way, and the sort is unmetered (below), so
  eliding it does **not** move the cost. The observable contract is the **row order** (the scan
  already delivers it). `query/order_by_pk_scan.test` pins the composite-key order.
- **Cost — with `LIMIT`.** The deliberate change: the scan short-circuits once the window is filled,
  so `storage_row_read` (and the filter `operator_eval`s) drop to the rows actually read — a top-N
  early-out, the same drop as the no-`ORDER BY` `LIMIT` short-circuit. `query/limit_offset.test` pins
  `ORDER BY id LIMIT 2` at the short-circuited cost (and the non-PK `ORDER BY val LIMIT 2` at the
  full-scan cost, the contrast).
- **The collation payoff.** A collated PK (or any collated key the scan walks) is stored in collation
  order, so a collated `ORDER BY` is satisfied **without** the in-memory collated decorate-sort (and
  with **no** `collate` units — there is no ordering *comparison*, just the scan emitting in stored
  order). `collation/collated_pushdown.test` pins `ORDER BY name LIMIT 2` over a `unicode` PK.
- **Composes with the PK bound** the same way the `LIMIT` short-circuit does: a `WHERE pk <range>`
  forward range walk is already PK order, so the bound narrows *which* rows are scanned and the
  `ORDER BY` still streams within it.

Narrowings (each a follow-on optimization slice): `DESC` (reverse scan), **secondary-index** order
(walk the index tree + point-lookup — the general non-PK collation payoff), `DISTINCT`, and multi-
table joins all keep the blocking sort / eager path.

### `SELECT DISTINCT` — the projection-vs-produce asymmetry

`DISTINCT` ([grammar.md](grammar.md) §11) deduplicates the **projected** output, so it must
project *every* filtered row to compute its dedup key — there is no way to know a row is a
duplicate without evaluating its select list. That splits two charges the un-`DISTINCT` path
keeps together:

- **Projection `operator_eval` is charged per *filtered* row**, not per windowed row — for
  each filtered row, every interior projection node fires once. This is independent of
  `LIMIT`/`OFFSET` and of how many rows turn out to be duplicates; the work is genuinely
  done. (Leaf `column`/constant projections still charge nothing, so a bare-column
  `SELECT DISTINCT a` adds no projection cost at all.)
- **`row_produced` is charged per *emitted* row** — the rows surviving dedup **and** the
  window — unchanged from its "one per row in the result set, post-`LIMIT`/`OFFSET`"
  definition (now also post-`DISTINCT`). So `row_produced` always equals the output row
  count.
- **Dedup itself is unmetered**, like the `ORDER BY` sort and the `LIMIT` slice (a dedicated
  dedup-comparison unit could be added later, as for the sort).

A consequence worth stating because it is observable and is a cross-core abort-point contract
(§6): because all filtered rows are projected, a projection that traps fires **even under a
`LIMIT` that would exclude the offending row**. `SELECT DISTINCT 1/a FROM t LIMIT 1` traps
`22012` if *any* filtered row has `a = 0`, whereas un-`DISTINCT` `SELECT 1/a FROM t LIMIT 1`
windows first and does not. The trapping row is deterministic (primary-key scan order), so
all three cores trap identically.

### JOIN — multi-table FROM (the nested-loop contract)

A multi-table `SELECT` ([grammar.md](grammar.md) §15) is a **left-deep nested-loop** join. Its
cost is pinned here because, with no reference implementation, the count is a cross-core contract
(§1). Three rules, each a small extension of the single-table rules above:

- **`storage_row_read` is charged once per physical row as each base table is materialized** —
  total = the **sum of the table cardinalities** (`|A| + |B| + …`), independent of join order or
  fan-out. A row is pulled from its store exactly once (each table is scanned into memory in
  primary-key order); the nested loop then re-reads from that **in-memory** buffer, which is not a
  store and charges nothing. This keeps the existing rule verbatim ("once per row pulled from a
  store, in the executor loop not the storage iterator" — so the Rust lazy-iterator vs Go/TS
  materialized-slice split stays neutralized) and keeps single-table cost identical (one table →
  its cardinality). When a table is **bounded** by a WHERE predicate on its own primary key
  (`query.join_pushdown`, "Bounded scan / JOIN" above), only its in-range rows are materialized, so
  its `storage_row_read` (and `page_read`) is the bounded count, not the full cardinality — a miss
  materializes zero. The bound never changes the result, only which rows are scanned.
- **The `ON`-predicate `operator_eval` is charged per candidate combination** the join evaluates
  it against — for an `INNER JOIN`, once per (running-row × right-row) pair, the `ON` tree's
  interior nodes firing pre-order with **no short-circuit**, exactly like a WHERE. A `CROSS JOIN`
  has no `ON` and charges no join `operator_eval` (it keeps every pair). So `ON` cost =
  |running| × |right| × (interior nodes in the `ON`), deterministic and fan-out-explicit. The
  iteration order — running/left side outer in PK order, right side inner in PK order, left-deep —
  is fixed so the per-combination evals accrue in the same sequence in every core (a §8 surface;
  it fixes the cost-ceiling abort point even though only the total is asserted today).
- **WHERE `operator_eval`** is charged per **surviving combined row** (post-join), and
  **`row_produced`** per emitted output row (post-`LIMIT`/`OFFSET`) — both unchanged; the combined
  row is simply wider. Join materialization buffering, the nested-loop control flow, and row
  concatenation are **unmetered**, like the `ORDER BY` sort and the `LIMIT` slice.

**Worked example.** Tables `a` (3 rows), `b` (2 rows), each small enough to be a single leaf
page; `SELECT * FROM a JOIN b ON a.k = b.k`, with 2 pairs surviving the `ON`. Materialize `a` →
1 `page_read` + 3 `storage_row_read`; materialize `b` → 1 + 2; the `ON` (`a.k = b.k`, one
interior `compare` node — its operands are leaf columns, charging nothing) over 3 × 2 = 6
candidate pairs → 6 `operator_eval`; no WHERE; `*` is bare-column projection (leaves, charge
nothing); 2 emitted rows → 2 `row_produced`. **Total = (1 + 3) + (1 + 2) + 6 + 2 = 15.** A
`CROSS JOIN` of the same tables emits all 6 pairs and evaluates no `ON`: 1 + 3 + 1 + 2 + 0 + 6 =
**13**.

**OUTER joins charge identically — only the produced-row count grows.** `LEFT`/`RIGHT`/`FULL [OUTER]
JOIN` ([grammar.md](grammar.md) §15) evaluate the `ON` over the **same** `|running| × |right|`
candidate set (so the `ON` `operator_eval` count is unchanged from an INNER join of the same tables);
a row that matches nothing is then **NULL-extended on the absent side and added to the surviving set
without re-evaluating `ON`** — the NULL-extension itself is unmetered, like row concatenation. Those
NULL-extended rows are ordinary surviving combined rows, so they incur WHERE `operator_eval` and
`row_produced` exactly like matched rows. So for the example tables with `SELECT * FROM a LEFT JOIN b
ON a.k = b.k` where 1 `a`-row matches 1 `b`-row and the other 2 `a`-rows match nothing: 1 + 3 and
1 + 2 to materialize (one leaf page each), 6 `ON`, no WHERE, and 1 matched + 2 NULL-extended = 3
emitted rows → **(1 + 3) + (1 + 2) + 6 + 3 = 16** (the INNER form of the same query is
`… + 1 = 14`; the +2 is the two preserved-left rows).

### FROM-less `SELECT` — the virtual row charges no scan units

A `SELECT` with no `FROM` clause ([grammar.md](grammar.md) §34) evaluates its select list over
**one virtual zero-column row**. There is no relation, so **no scan units accrue** — zero
`page_read`, zero `storage_row_read`, zero `value_decompress`. The virtual row then flows
through the ordinary clause rules above: the `WHERE` predicate charges its `operator_eval`s,
aggregation charges `aggregate_accumulate` per (input row × aggregate) over the single row, and
each emitted row charges `row_produced`. So `SELECT 1` costs exactly **1** (one `row_produced`;
a literal projection is a leaf), `SELECT 1 + 2` costs **2**, `SELECT 1 WHERE false` costs **0**
(a constant filter is a leaf and no row is produced), and `SELECT count(*)` costs **2**
(1 `aggregate_accumulate` + 1 `row_produced`). As a set-operation operand, a subquery, or an
`INSERT … SELECT` source it composes by the rules below with no special case.

### `generated_row` — a set-returning function's computed rows

A set-returning function in the `FROM` clause (`generate_series` — [functions.md](functions.md)
§10, [grammar.md](grammar.md) §35) is a **computed** row source, not a scanned table: it touches
no page and reads no stored row, so it charges **no** `page_read` and **no** `storage_row_read`.
Instead each element the generator emits charges one **`generated_row`**, accrued **at the source**
(before the row enters the join/WHERE pipeline) with a `guard()` first — so a runaway
`generate_series(1, 10^18)` aborts deterministically with `54P01` once accrued cost reaches the
ceiling, **mid-generation**, never materializing the whole series (CLAUDE.md §13). The arguments
are evaluated once up front, each charging its own `operator_eval`.

`generated_row` meters **generation**; `row_produced` meters **emission** into the final result.
They are deliberately distinct and **diverge under `WHERE` / `LIMIT`**: a generated row that a
`WHERE` filters out, or that an enclosing join/limit never emits, still charged `generated_row`
but charges no `row_produced`. Worked examples (all asserted in the corpus):

- `SELECT * FROM generate_series(1, 5)` — 5 `generated_row` + 5 `row_produced` = **10**.
- `SELECT * FROM generate_series(1, 5) WHERE generate_series > 2` — 5 `generated_row` + 5
  `operator_eval` (the `>` per generated row) + 3 `row_produced` = **13**.
- `SELECT * FROM generate_series(1, 5) ORDER BY generate_series DESC LIMIT 2` — 5 `generated_row`
  + 2 `row_produced` = **7** (the SRF takes the eager path; the sort/limit are unmetered).
- `SELECT * FROM t CROSS JOIN generate_series(1, 3)` over a 3-row `t` — `t`'s scan block
  (`page_read` + 3 `storage_row_read`) **+ 3 `generated_row`** (the series is materialized once,
  like any join operand) + 9 `row_produced` for the product.

### `sequence_advance` — a sequence mutation

`nextval('s')` (and, from S2, `setval`) advances a sequence's catalog tuple, more than a pure
value→value map, so it charges one **`sequence_advance`** unit **in addition** to the one
`operator_eval` every function call rides ([sequences.md](sequences.md) §8). The catalog-tuple
read+rewrite is schema-bounded (a fixed `SequenceDef`), so a flat per-call weight is a sound bound;
it keeps a runaway `nextval` (e.g. `SELECT nextval('s') FROM generate_series(1, 10^18)`) bounded by
`max_cost` — the `54P01` ceiling aborts deterministically. `currval('s')` reads only per-session
state and charges nothing beyond its `operator_eval`. Worked example:

- `SELECT nextval('s')` — 1 `operator_eval` (the call) + 1 `sequence_advance` = **2** (plus 1
  `row_produced` for the result row).

A correlated SRF argument (`generate_series(1, o.n)` inside a subquery) re-evaluates its arguments
and re-generates per outer row, exactly like a correlated subquery's inner re-scan (the Subqueries
subsection) — so the generated rows accrue per outer row.

### `cte_scan_row` — a materialized CTE's buffered rows, and the inline path

A common table expression ([cte.md](cte.md)) is a named, statement-local relation backed by a
planned query. jed evaluates it by PostgreSQL's **hybrid rule**, and the rule *is* the cost
contract: it decides whether a CTE's body runs once-and-is-buffered or runs in place.

- **Inlined** (referenced exactly once, not `MATERIALIZED`): the body runs **in place** at the
  FROM position, like a derived table. It charges exactly its **intrinsic** cost — the
  `page_read` / `storage_row_read` / `operator_eval` / `generated_row` / `row_produced` its plan
  accrues — once per scan of that relation. Under a correlated subquery it re-runs per outer row
  (the body re-executes), exactly like an inlined subquery's inner re-scan. No new unit; a single
  reference costs the same as writing the body inline. A **derived table** (`FROM (SELECT …) AS t`,
  [grammar.md §42](grammar.md#42-derived-tables-from--query_expr--as-t)) takes this same inline path
  — it has no name to reference twice, so it is always inlined and never charges `cte_scan_row`.
- **Materialized** (referenced ≥ 2 times, or `MATERIALIZED`): the body runs **once**, accruing its
  full intrinsic cost into a row buffer, and **each reference** charges one **`cte_scan_row`** per
  buffered row — accrued **at the source** (before the row enters the join/WHERE pipeline) with a
  `guard()` first, so a runaway scan aborts `54P01` deterministically. The buffer is a computed
  source, not a table store, so a buffer scan charges **no** `page_read` and **no**
  `storage_row_read` — the `generated_row` precedent: stored tables charge page+row, computed
  sources charge their own per-row unit.

`cte_scan_row` meters a **buffer read**; `row_produced` meters **emission** into the final result —
distinct and divergent under `WHERE`/`LIMIT`, like `generated_row`. The whole formula:

> `cost(WITH … main) = Σ_referenced cost(body, once) + Σ_each_materialized_reference (|buffer| ×
> cte_scan_row) + cost(main pipeline)`

A CTE **body is a query** — it runs through the ordinary query pipeline, so its result rows charge
`row_produced` exactly as a scalar subquery's folded result does (the Subqueries subsection: a
`(SELECT max(k) …)` charges `row_produced` for its one result row). The outer query then charges
`row_produced` again for **its** final rows. This layering is deliberate and deterministic — an
inlined CTE costs *more* than the same query written without the `WITH`, by the body's
`row_produced`; jed's cost is its own cross-core contract, not PG's. An **unreferenced** CTE is
planned and type-checked but **not executed** — it adds **0** exec cost. A CTE referenced *k* times
(materialized) charges its body cost **once** but `k × |buffer|` `cte_scan_row`. Worked examples
(asserted in the corpus), over a 3-row `t` whose B-tree is a single node:

- `WITH c AS (SELECT * FROM t) SELECT * FROM c` — **inlined** (one reference): body = `page_read`
  (1) + 3 `storage_row_read` + 3 `row_produced` = **7**; the outer scans the computed relation `c`
  (no store, no `cte_scan_row`) and emits 3 rows = 3 `row_produced`. Total **10**.
- `WITH c AS (SELECT * FROM t) SELECT * FROM c a CROSS JOIN c b` — **materialized** (two
  references): body once = **7**, then `3 + 3 = 6` `cte_scan_row` for the two buffer scans, then 9
  `row_produced` for the 3×3 product. Total **22**.
- `WITH c AS (SELECT * FROM t) SELECT 1` — `c` unreferenced: **0** cost for `c`; only the
  FROM-less `SELECT 1` (1 `row_produced`). Total **1**.

The materialization runs **between plan and exec**, accruing into the **running statement cost** (a
seed carried forward, the same accrued-seed mechanism set operations use below) — never a per-CTE
meter that resets the ceiling, so the `54P01` abort point during materialization is cross-core
identical (CLAUDE.md §8/§13).

A **data-modifying CTE** ([writable-cte.md](writable-cte.md)) charges no new unit: it accrues its
**intrinsic DML cost** once into the running total (the rows it scans / its expressions / one
`row_produced` per `RETURNING` row — exactly a standalone `INSERT`/`UPDATE`/`DELETE`), and each
reference to its buffer charges `cte_scan_row` per buffered row, like any materialized CTE. It is
**always** materialized and runs to completion (even unreferenced — its scan/write-validation cost
still accrues, so a side-effect-only one is not free); the meter stays continuous across every
sub-statement of the `WITH`, so a `54P01` ceiling trips at the identical accrued cost in every core.

### Set operations — `lhs + rhs`, the combine unmetered

A set operation ([grammar.md](grammar.md) §25) — `UNION`/`INTERSECT`/`EXCEPT`, each with an
optional `ALL` — combines the result sets of two operand queries. Its cost is the **sum of the
operand costs and nothing more**:

> `cost(a ⊕ b) = cost(a) + cost(b)`

Each operand is a full `select_core` (or a nested set operation) run through the ordinary query
path, so it **already** charges `storage_row_read` per scanned row, the `operator_eval`s of its
own clauses, and `row_produced` per row it emits (its *pre-combine* output). The set-operation
layer then consumes those materialized rows and does **only set-membership work** — match rows by
the NULL-safe value-canonical key, take the multiset union / intersection / difference, emit the
representative rows — which is **unmetered**, exactly like `DISTINCT` dedup (above), the
`ORDER BY` sort, and the `LIMIT`/`OFFSET` slice. The trailing `ORDER BY` and `LIMIT`/`OFFSET` of a
set operation are likewise unmetered (§ "What is NOT metered"). The integer→`decimal` value
conversion that type unification may apply before keying (§25) is structural, like a JOIN's
NULL-extension, and charges nothing. **No new cost unit** is introduced.

This **follows the `INSERT … SELECT` precedent** (§24, where the wrapping statement adds nothing
to the embedded `SELECT`'s cost), not the single-`SELECT` shape. A deliberate consequence: the
`DISTINCT` invariant "`row_produced` equals the output row count" **does not hold** for a set
operation — the operands charge `row_produced` for their *pre-combine* rows, and the combine that
drops/duplicates rows is unmetered, so the accrued `row_produced` reflects what the operands
produced, not the set operation's final output. This is correct and intended: cost composes from
the independently-metered subqueries.

**Worked example.** Tables `a` (3 rows) and `b` (2 rows), each a single leaf page;
`SELECT x FROM a UNION SELECT x FROM b`. The left operand materializes `a` → 1 `page_read` +
3 `storage_row_read` and emits 3 rows → 3 `row_produced` (a bare-column projection is a leaf,
charging no `operator_eval`): 7. The right operand: 1 + 2 + 2 = 5. The `UNION` dedup is
unmetered. **Total = 7 + 5 = 12**, whatever the number of distinct output rows. `UNION ALL`
(no dedup) costs the **same** 12 — the dedup was already free, so dropping it changes nothing.
The cross-core contract is trivially identical: it is literally the sum of two
independently-deterministic operand costs.

### Subqueries — initplan once, correlated per outer row

A subquery ([grammar.md](grammar.md) §26) — scalar `(SELECT …)`, `x IN (SELECT …)`, or
`EXISTS (SELECT …)` — composes its operand query's cost into the enclosing query with **no new
cost unit**. The subquery runs through the ordinary query path, so it **already** charges its
own `storage_row_read` / `operator_eval` / `row_produced` exactly as any `SELECT` does; the
folding/membership/cardinality machinery is **unmetered**, like `DISTINCT` dedup and the
set-operation combine. How many times that operand cost lands depends on correlation:

- **Uncorrelated** (an "initplan") — executed **exactly once**, at plan setup, and folded into a
  constant. Its cost is added **once**, and the folded constant is a **leaf** (charges no
  `operator_eval` when the outer row evaluates), so a scalar subquery referenced once in `WHERE`
  adds its operand cost once, not once per outer row:

  > `cost(query with uncorrelated s) = cost(query) + cost(s)`

  A globally-uncorrelated subquery is folded once **even when it is nested inside a correlated
  one** (its value never changes), so it too is counted once.

- **Correlated** — re-executed once **per outer row** that reaches its expression node, reading
  the enclosing-row values its plan references. Each execution adds that execution's full
  operand cost (which can vary per outer row, since the correlated values filter the inner scan
  differently), and the subquery node itself — being a real interior operator now, not a folded
  leaf — charges **one `operator_eval`** each time it evaluates. A correlated `IN` additionally
  charges one `operator_eval` per inner result value its membership test compares (the §26 IN
  model). So for a correlated subquery `s` reached by outer rows `R`:

  > `cost(query with correlated s) = cost(query) + Σ_{r ∈ R} (operator_eval + cost(s | r))`

  When the inner query is a single table whose WHERE bounds its **primary key** by an enclosing
  column (`inner.pk = o.col`), each `cost(s | r)` is the **bounded** inner scan for that outer
  row's value — a per-outer-row point lookup/range, not a full re-scan (see "Bounded scan /
  correlated" above; `query.correlated_pushdown`). The Σ shrinks accordingly, but the formula is
  unchanged — only each term is smaller.

Both are fully deterministic and identical across cores: the same `(query, database)` always
visits the same outer rows in the same order and runs the subquery the same number of times.

The same accounting applies when the enclosing statement is a **`DELETE` / `UPDATE`** (a
subquery in its `WHERE`, or an `UPDATE` assignment RHS — grammar.md §26): an uncorrelated
subquery folds once (operand cost added once, before the scan), and a correlated one re-runs
per **scanned** row that reaches its node, adding `operator_eval + cost(s | r)` each time —
identical to the `SELECT` case, since both mutations drive the same per-row evaluator. The
phase-2 writes evaluate nothing and stay unmetered (below).

### `RETURNING` — DML that produces rows

A DML statement's `RETURNING` clause ([grammar.md](grammar.md) §32) is metered as a
**`SELECT` projection over the affected rows**, with no new cost unit:

- **Per returned row**: one `row_produced` plus the item expressions' metered evaluation
  (`operator_eval` per interior node; `decimal_work` composes) — exactly the charge a
  `SELECT` makes when it emits a row. `RETURNING *` and bare column references are leaves
  (`row_produced` only). The statement's existing charges (scan block, per-row
  `storage_row_read`, filter/assignment/check evaluation, `value_compress`) are unchanged,
  and a statement that affects zero rows charges nothing for its `RETURNING`.
- **The touched set** (the subsection above) **grows by the items' column references** for
  the statements that read stored rows, and the `old.`/`new.` qualifiers (grammar.md §32)
  distinguish the sides:
  - a `DELETE`'s touched set becomes `WHERE ∪ RETURNING(old side)` — bare and `old.`
    references read the dropped row; a `new.col` is the constant NULL row and reads
    **nothing**;
  - an `UPDATE`'s becomes `WHERE ∪ assignment sources ∪ (new side ∖ assigned columns)
    ∪ old side` — an **assigned** column's new value is the freshly computed one (not a
    storage read), but its `old.col` is **always** a storage read, assigned or not;
  - an `INSERT`'s `RETURNING` reads no stored row at all (the new side is the statement's
    own candidates, the old side the constant NULL row), so it never adds scan units; an
    `INSERT ... SELECT`'s source charges through its own query path as before.
- **Subqueries in the list** follow the subsection above: uncorrelated folds once (cost
  added once, evaluated against the pre-statement snapshot — grammar.md §32), correlated
  re-runs per **returned** row (`operator_eval + cost(s | r)` each).
- **Ordering / the ceiling**: projections evaluate after the statement's validation
  completes and **before any write**, charging per returned row in scan order with the
  per-row ceiling guard — so a `54P01` abort mid-`RETURNING` has written nothing
  (all-or-nothing is preserved; §6).

### What is NOT metered (defined boundary)

Metering covers **execution** — per-row scans, per-row produced, per-row expression
evaluation. It deliberately does **not** meter:

- **Parse / plan / resolve** — these are per-statement (and the literal range-checks,
  type resolution, etc. happen once), not per-row execution.
- **`ORDER BY` sort-internal comparisons** — the sort compares `Value`s directly, not
  through the expression evaluator, so they are outside the `operator_eval` unit (and the
  `decimal_work` unit — `MIN`/`MAX` folds are the same direct compare and share this
  boundary). This holds for a **multi-key** sort too (each key's comparison is the same
  direct `Value` compare), so adding keys or `NULLS FIRST|LAST` placement changes no cost. It
  also holds when the sort **spills to disk** under `work_mem` ([spill.md](spill.md)): the
  external merge sort's run-spill / k-way-merge comparisons and its temp-file I/O are
  sort-internal, so a larger-than-RAM `ORDER BY` charges exactly what the in-memory sort did —
  cost is invariant to whether and how often it spilled (spill.md §6).
  (A dedicated sort-comparison unit could be added later if wanted; it is not in this slice.)
- **`LIMIT` / `OFFSET` slicing** — selecting the output window is an index slice over the
  already-sorted rows, not evaluation work; like the sort it is unmetered. Its only cost
  effect is *fewer* `row_produced`/projection charges (the excluded rows are never
  projected — see the `row_produced` rule above).
- **`DISTINCT` dedup** — testing whether a projected tuple has been seen is set membership,
  not evaluation, so it is unmetered like the sort and the slice. Its cost effect is the
  asymmetry above: projection `operator_eval` is charged for every filtered row, but
  `row_produced` only for the surviving distinct, windowed rows.
- **Phase-2 row writes** in `UPDATE`/`DELETE` — the two-phase mutation's write pass does
  no eval and produces no row.
- **Secondary-index maintenance** — computing and writing/removing index entries at
  INSERT/UPDATE/DELETE is phase-2 write work (it evaluates nothing and cannot fail), so it
  is unmetered like the row writes themselves. The *build* scan of `CREATE INDEX` over
  existing rows **is** metered (the index-bounded-scan subsection above); `DROP INDEX`
  charges 0.
- **Uniqueness validation** — the primary-key duplicate check and the unique-index probes
  (indexes.md §8) at INSERT/UPDATE, and `CREATE UNIQUE INDEX`'s build verification, are
  constraint validation like NOT NULL (a branch, not expression evaluation): unmetered. An
  INSERT into a uniquely-indexed table costs the same as into a plainly-indexed one.
- **JOIN nested-loop control flow** — buffering each materialized table, iterating the
  Cartesian/left-deep combinations, and concatenating left+right rows are bookkeeping, not
  evaluation; only `storage_row_read` (per materialized row), the `ON`/WHERE/projection
  `operator_eval`s, and `row_produced` accrue (see the JOIN subsection above).
- **Set-operation combine** — matching rows by the NULL-safe value-canonical key, the multiset
  union/intersection/difference, the integer→`decimal` unification conversion, and the trailing
  `ORDER BY`/`LIMIT`/`OFFSET` are all set-membership / bookkeeping, not evaluation; a set
  operation accrues only its operands' costs (`lhs + rhs`, see the set-operations subsection
  above).

## 4. Counter representation — exactness across cores (CLAUDE.md §8)

The accrued cost is carried as a signed 64-bit integer: `i64` (Rust), `i64` (Go),
**`bigint` (TS)**. TS must use `bigint`, not `number`: a `number` is an IEEE-754 `f64`,
and a large scan crosses 2^53 where `f64` loses integer precision, silently diverging
from the Rust/Go `i64` totals — exactly the §8 hotspot the type system exists to kill.
The TS core already carries i64 values as `bigint`, so this is consistent. Cost renders
as a plain shortest-decimal integer, matching the `# cost: N` corpus directive.

## 5. The seam shape (so enforcement is additive)

Every accrual routes through a single `Meter::charge(units)` chokepoint per core (a tiny
`Meter` struct threaded by `&mut`/pointer/mutable-object through the executors and the
recursive evaluator). The accrued total is exposed on `Outcome` (both the statement and
query variants — a `DELETE` still accrues scan + filter cost). Centralizing accrual in
`charge`, with the ceiling check factored into `Meter::guard()`, is what kept enforcement a
local change (§6).

## 6. Enforcement — the cost ceiling (landed)

The metering seam (§5) exists so that bounding an untrusted query is a small, local addition.
It is now built:

- **Caller-set ceiling.** The handle carries a `max_cost` setting (spec/design/api.md §8),
  `0` (the default) ⇒ **unlimited**, a positive value ⇒ the ceiling. Each statement's `Meter`
  is constructed with that limit. It is a handle setting, not stored in the file — the host
  configures the budget for whatever handle serves untrusted queries.
- **Deterministic abort via `guard()`.** `charge` stays a pure accrual chokepoint (so the
  `# cost:` accrual contract is **byte-unchanged**); a separate `Meter::guard()` does the
  comparison and **aborts when accrued cost has reached the ceiling** (`accrued >= limit`,
  CLAUDE.md §13 — "the instant accrued cost reaches it"). The ceiling is therefore the first
  *disallowed* value: a query whose true cost equals the ceiling aborts, one costing
  `ceiling − 1` completes. `guard()` is consulted at the **unbounded-work points** — once per
  scanned row (the SELECT/JOIN materialization, the DELETE and UPDATE scans, the streaming
  LIMIT walk), once per produced row, once per expression node (the recursive evaluator's
  entry), once per aggregate fold row, and **immediately after each size-scaled
  `decimal_work` (or `varlen_compare`) charge** (§3 — so the ceiling aborts *before* the
  big-decimal limb work, or the long text/bytea byte scan, runs — not at the next node). These
  points are **mirrored identically across
  Rust, Go, and TS**, and accrual order is fixed (§3), so the abort is deterministic and
  **cross-core identical**: the same `(query, db, ceiling)` aborts (or completes) in every
  core. A subquery executes through the same path with the same `max_cost`, so a runaway
  correlated re-scan aborts within its own execution; the outer meter additionally accrues the
  subquery's cost (`charge(r.cost)`), so the outer scan guard sees the running total. The
  guard is a single comparison and a **no-op when unlimited**, so it is free on the hot path
  by default.
  - **Surfacing differs per core, the abort point does not.** Rust returns `Result` (the
    guard is `m.guard()?`), Go returns `error` (`if err := m.Guard(); err != nil`), TS
    **throws** the `EngineError` (which unwinds to the API boundary like every other SQL
    error) — each its own idiom, all aborting at the same guarded point. The abort is an
    **ordinary engine error**, so it flows through the existing rollback-on-error paths
    untouched: an aborted autocommit DELETE/UPDATE discards its working set and leaves the
    table unchanged, and inside an explicit block the abort poisons the block (§transactions).
  - **Bounded overshoot, by design.** Because `guard()` is checked at the work-loop
    boundaries rather than inside every `charge`, accrued cost can pass the ceiling by at most
    the work of one unit between two guards — one row's filter/projection, one expression
    subtree, or one folded row (the membership loop over an `IN`-subquery's result is bounded
    by that result, which a guarded inner scan already capped). The overshoot is itself
    deterministic and cross-core identical. Tightening `page_read`'s single up-front block
    charge, and a single global running counter across subquery nesting, are possible later
    refinements; neither changes the abort *decision* for a `(query, db, ceiling)`.
- **Cost-ceiling error code — `54P01` `cost_limit_exceeded`.** Authored in
  [../errors/registry.toml](../errors/registry.toml), class `54` *program_limit_exceeded* (a
  caller-imposed limit was exceeded). jed-specific — PostgreSQL has no execution-cost ceiling,
  so it is a documented divergence (CLAUDE.md §1/§13), the `P` subclass marking it as jed's,
  like the existing `22P02`/`42P18`/`25P02`.
- **Conformance.** The `# max_cost: N` directive (mirroring `# cost:`) runs the next record
  under a ceiling of N; an over-ceiling record is `statement error 54P01`, an under-ceiling
  record runs normally and may assert its `# cost:`.
  [../conformance/suites/resource/cost_limit.test](../conformance/suites/resource/cost_limit.test)
  pins both directions cross-core, gated by the `resource.cost_limit` capability — the single-table
  scan / point-lookup / DELETE / UPDATE / RETURNING cases plus the exact first-disallowed boundary.
  [../conformance/suites/resource/dos_amplification.test](../conformance/suites/resource/dos_amplification.test)
  extends it to the **work-amplifying shapes** an untrusted query reaches for — a cartesian product
  (`row_produced` per emitted row, so N-way self-joins are bounded), a correlated subquery (the inner
  relation re-scanned per outer row), and a big-decimal multiply (the `decimal_work` guard aborts
  *before* the limb loop, §3) — each stopped by the same 54P01 abort. (The runaway set-returning
  function — `generate_series` over a giant range — is pinned mid-generation in
  [../conformance/suites/query/generate_series.test](../conformance/suites/query/generate_series.test).)

Other items recorded against the seam:

- **A real `page_read` unit — ✅ landed (P6.3).** The store is now a page-backed B-tree
  ([storage.md](storage.md) §6), so a distinct `page_read` unit was **added** to the schedule
  (not a rename of `storage_row_read` — both fire on a scan) and is charged per node a scan
  touches. It counts a **logical** page access (the tree's structural node count), **not** a
  physical disk fetch, so the future buffer pool / cache for larger-than-RAM files
  (CLAUDE.md §9) cannot perturb the deterministic, cache-independent cost (§13). Accrual
  rules: §3 "`page_read`".
- **Per-operator `cost` weights — ✅ live.** The per-operator `cost` field in
  [../functions/catalog.toml](../functions/catalog.toml) ([functions.md](functions.md) §8) is now
  codegen'd into `OperatorDesc` and **read by the evaluator**: an operator node charges
  `operator_cost(name)` — the operator's own `cost` base if authored, else the uniform
  `operator_eval`. It is a **name-level, size-independent** base — the size-scaled units
  (`decimal_work` / `varlen_compare` / …) carry argument-dependent cost. **No built-in sets a
  non-default `cost`**, so every weight is still the uniform `operator_eval` and cost is unchanged;
  but tuning a built-in's base (or a host function's static weight, below) is now a **pure data
  change** in catalog.toml. The evaluator reads it at the arithmetic / comparison / logical arms; the
  override lookup is empty-fast-pathed so the all-default case adds no per-node work. (The per-core
  unit tests — `operator_cost_reflects_catalog` and the catalog cross-checks — pin the data-driven
  lookup the uniform-weight corpus cannot observe.)
- **Host-defined functions must contribute cost (open requirement).** When host-defined
  functions land (CLAUDE.md §2; TODO.md Phase 7/9), they are **opaque to the meter** by
  default — host code does not route through `charge` — which would break both the
  untrusted-query bound (§6/§13: an unmetered call burns unbounded CPU past `max_cost`) and
  the **cross-core cost identity** §8 demands (a wrapped core and a native core must compute
  the *same* cost for the same call). The host-function registration API must therefore carry
  a cost contract, one of: (a) a **declared static weight** (charged once per call, like
  `operator_eval` — the host-function generalization of the now-live per-operator `cost` field,
  [functions.md](functions.md) §8); (b) a **declared deterministic cost function of the argument
  values/sizes**, charged up front and guarded *before* the call (the `decimal_work` /
  `varlen_compare` / `value_compress` model above); or
  (c) a **deterministic metering callback** — a narrow `charge(n)` handle into the `Meter`
  that the host calls as it works, enabling a chunk-boundary **mid-call abort** (the per-chunk
  model). Whichever is chosen, it must be deterministic and cross-core identical — **no
  wall-clock**, no allocation/iteration-order basis (§10, [storage.md](storage.md)). A host
  function supplying none of these is admissible only on a handle with `max_cost = 0`
  (unlimited), never the untrusted-query surface. Tracked in TODO.md (Phase 7/9).

## 7. Native-stack safety — the expression nesting-depth limit (landed)

The cost ceiling (§6) bounds *accrued cost*, but it does **not** bound **native call-stack
depth** — and that is a second, independent §13 untrusted-query hazard. The parser is
recursive-descent and the downstream walks (resolve `Expr`→`RExpr`, evaluate, constant-fold,
the touched-column / structural-validation passes) recurse to a statement's **nesting depth**.
So deeply-nested untrusted input — `1 + 1 + … + 1` thousands deep, or nested
parens / `ARRAY[…]` / subscripts / `CASE` / scalar subqueries — can **overflow the call
stack during parse or resolve, *before any cost is metered***. The cost ceiling cannot catch
this: such a statement SIGABRTs (or, in the TS core, throws an uncatchable-by-design
`RangeError`) **even at `max_cost = 1`**, because the overflow happens before the meter runs.
Memory-safety (§13) does not catch it either — a stack overflow is an abort, not a memory
error the safe language prevents. So it needs its own gate.

**The fix — a fixed maximum nesting depth, checked in the parser.** A single shared
**depth counter** is threaded through the recursive-descent expression grammar and incremented
once at each point the AST gains a level: every binary-operator chain step (`OR`/`AND`/
additive/multiplicative loops), every unary (`NOT`, unary `-`), every postfix
(`::`cast / `[…]`subscript / `.field`), every re-entry into a fresh expression (parenthesized
sub-expression, `ARRAY`/`ROW`/function-argument/`CASE`/subscript-index operand), every nested
**scalar subquery / `EXISTS` / `IN (SELECT …)`**, and every **set-operation** branch
(`UNION`/`INTERSECT`/`EXCEPT` chain). When the counter exceeds **`MAX_EXPR_DEPTH = 256`** the
parser aborts with **`54001` `statement_too_complex`** ([../errors/registry.toml](../errors/registry.toml)).

Why enforce in the **parser** and not the evaluator: the parser is the *first* pass and the
*producer* of the tree. Bounding the depth there means **no `Expr`/`RExpr` taller than the
limit is ever constructed**, so every downstream walk (resolve, eval, fold, the structural
`CHECK`/`DEFAULT` validators, the touched-column collector) is transitively safe with **zero
extra guard sites** — one mechanism, one place. (`IN (list)`, `BETWEEN`, `LIKE`, `CASE` are
flat `RExpr` nodes, not desugared into deep nesting, so bounding source-AST depth bounds the
resolved tree to within a constant — the bound is tight.)

**Why `256`, and why a fixed number rather than PG's probe.** PostgreSQL raises this same
`54001` from `check_stack_depth()`, which compares the *actual* stack pointer against
`max_stack_depth` — a value that depends on the build, the platform, and the per-frame size, so
it is **non-deterministic and not cross-core reproducible**. jed instead counts **logical
nesting depth** against a **fixed** limit: deterministic, identical in every core, independent
of build mode (debug vs. release) and platform — exactly the §8 cross-core-identity contract
(the same `(statement)` is accepted or rejected with `54001` in Rust, Go, and TS alike). The
constant is chosen for **native-stack headroom in the *weakest* core**: the TS core, running on
a default Node/V8 call stack, overflows at roughly ~547 nested subqueries, ~860 operator-chain
levels, and ~940 nested parens; the Rust and Go cores tolerate far more (Go's stack grows;
Rust release frames are small). `256` sits with a **>2× margin** under the tightest of those
(nested subqueries, which cost two depth units per level and so trip the limit at ~128 actual
levels), while remaining **far** above any realistic query — hand-written or ORM-generated
SQL does not nest expressions hundreds deep. It is a deliberate, documented divergence from
PG's effectively-larger limit (the *overriding reason*: cross-core determinism + the weakest
core's stack, CLAUDE.md §1/§8/§13), recorded here at the point it is taken.

**Determinism + cost.** The check is **free of cost units** — it is a structural bound on the
statement, not metered work, and it fires identically regardless of `max_cost`. The depth
counter resets per statement (a fresh parser per `parse_sql`). Surfacing follows each core's
idiom like every other engine error (Rust `Result`, Go `error`, TS `throw`), aborting at the
same logical depth.

**Conformance.** [../conformance/suites/resource/depth_limit.test](../conformance/suites/resource/depth_limit.test)
pins both directions cross-core (a just-under-limit statement runs; an over-limit one is
`statement error 54001`), gated by the `resource.depth_limit` capability. Because the trigger
is jed-specific (not PG's runtime probe), it is **not** oracle-checked.

**Composite-chain follow-on — ✅ landed (§7b).** One recursion vector is *not* bounded by the
expression/query-nesting counter **or** by the input-size limit below: **deeply-nested `CREATE
TYPE` composite chains** resolved at DDL time. The chain is built across *many* cheap, individually
short statements (`CREATE TYPE a AS (…)`, `CREATE TYPE b AS (x a)`, …), so the per-statement
input-size cap never sees it, and the depth is a property of the *catalog graph* (resolved at
use/codec time), not a single statement's AST — so the nesting counter never sees it either. It has
its own catalog-resolution depth gate — the composite-type nesting limit in §7b. Any future grammar
that recurses outside the expression / query-expression / set-op cascade is the same shape; both are
noted so the seam's coverage boundary stays explicit. (Nested `ROW(…)` in `INSERT … VALUES` is
already capped by an earlier engine error — `42601` at depth 2 — so it is not a vector.)

## 7a. Input-size and identifier-length limits (landed)

The nesting-depth gate (§7) bounds the parse tree's **height**; two sibling gates — in the same
§13 family, all checked *before any cost is metered* (the §6 ceiling cannot catch parse-time
work) — bound its **breadth/total size** and its **identifier length**. Together with §7 they
close the input-hardening vectors for untrusted SQL: a hostile statement can be neither too tall,
nor too wide/long, nor name anything unboundedly.

**Input-size limit — `max_sql_length` (a per-handle setting).** A statement whose input text
exceeds the handle's `max_sql_length` (in **bytes**) is rejected with **`54000`
`program_limit_exceeded`** at the handle's parse entry, *before* lexing — so an adversarial 1 GB
query cannot exhaust parse memory/CPU (parsing is O(input), and the parse tree is O(input)). It is
a **per-handle setting** like `max_cost` ([api.md](api.md) §8): the default is **1 MiB**
(`DEFAULT_MAX_SQL_LENGTH = 1 << 20`), generous for hand-written / ORM SQL yet bounding the tree to a
few MB; **`0` is unlimited**, a trusted caller's opt-out (e.g. a bulk load). The cap is the
**maximum allowed** length — a statement of exactly `max_sql_length` bytes runs, one byte over
aborts (inclusive max, like §7's "exceeds the maximum"). The byte length is the **UTF-8 byte
count** — Rust `&str::len`, Go `len(string)`, and the TS core's `TextEncoder`-measured length all
agree, so the cap accepts/rejects identically across cores (§8); a core counting UTF-16 units would
diverge on multi-byte input.

*This single cap subsumes the parse-tree-breadth / node-count vector.* jed is **single-statement
per call** (`parse_sql` parses one statement, then `expect_eof`), and nothing in the grammar
desugars super-linearly (`BETWEEN` → 2 nodes, `IN (list)` / `CASE` stay flat `RExpr` nodes — §7),
so the AST node count is `O(input bytes)`: every node consumes ≥ 1 token consuming ≥ 1 input byte.
Bounding the input bytes therefore bounds the node count to within a constant — a 1 M-column
`SELECT`, a 200 k-element `IN` list, or a giant `VALUES` is just *bytes*, stopped by the same gate,
**without a separate node counter**. (Identifier length, below, is likewise ≤ the input size, but
gets its own tighter gate because identifiers persist to the catalog and keys.)

**Identifier-length limit — `MAX_IDENTIFIER_LENGTH = 63` bytes (a fixed constant).** A single
identifier — table / column / type / alias / function name — longer than 63 bytes is rejected with
**`42622` `name_too_long`**. The check sits in the **lexer**, at the identifier-token *producer*
(the same "bound at the producer" reasoning as §7's parser gate), so it bounds **every** identifier
on **every** parse path and fires during tokenization, before the parser dispatches. Identifiers
are ASCII-only ([grammar.md](grammar.md) §3), so 63 bytes = 63 characters. **63** matches
PostgreSQL's `NAMEDATALEN − 1` boundary, but jed **errors** where PG silently *truncates* — jed has
no notices, and a silent truncation could collide two distinct names (a documented PG divergence,
CLAUDE.md §1). A fixed, cross-core-identical constant (§8), like `MAX_EXPR_DEPTH`.

**Conformance.** [../conformance/suites/resource/input_size_limit.test](../conformance/suites/resource/input_size_limit.test)
pins the input-size boundary cross-core via the `# max_sql_length: N` directive (a small cap makes
the boundary testable with tiny inputs; the 1 MiB default would need a > 1 MiB file), including a
multi-byte case that pins UTF-8 byte counting; gated by `resource.sql_length_limit`.
[../conformance/suites/resource/identifier_length.test](../conformance/suites/resource/identifier_length.test)
pins the 63/64-byte identifier boundary in several positions; gated by
`resource.identifier_length_limit`. Both are jed-specific, so **not** oracle-checked.

## 7b. Composite-type nesting-depth limit (landed)

§7's nesting counter and §7a's input-size cap both bound a **single statement** — its AST height
(§7) and its total byte size (§7a). One native-stack vector escapes both: a **composite-type chain**
built across *many* cheap statements (`CREATE TYPE c1 AS (x i32)`, `CREATE TYPE c2 AS (x c1)`, …).
Each statement is tiny (under the input cap) and shallow (under the nesting counter), but the chain's
**nesting depth grows without bound**, and that depth is a property of the *catalog graph* — resolved
at use/codec time, not visible in any one statement's AST. Every recursive walk **derived** from a
composite type (the value codec, the `eq3`/`lt3`/`gt3` comparator, `record_out`/`record_in`,
`resolve_col_type`) recurses to this depth, so a long enough chain overflows the native call stack —
the same hazard §7 guards for expressions, reached by a different door.

**The gate — a fixed `MAX_COMPOSITE_DEPTH = 32`, enforced at the two producers of catalog types.** A
composite type's *depth* is the length of its deepest chain of nested composites, counting itself: a
row of scalars is depth 1, and `cN AS (x c{N-1})` is depth N. An **array field counts as its element
type** — array levels are not composite levels (`composite_ref` looks through one array level the
same way the dependency-tracking and two-pass-load paths do), so `c31[]` contributes depth 31, not
32. The two producers:

- **`CREATE TYPE` (the in-scope §13 query gate) → `54001` `statement_too_complex`.** A new composite
  references only *existing* types (an unknown field type is `42704` — no forward references, so no
  runtime cycle), each of which already satisfies depth ≤ `MAX_COMPOSITE_DEPTH` (the invariant this
  gate maintains). So the new type's depth is `1 + max(field depths)`, computed by a small memoized
  recursion bounded by the limit; if it would exceed `MAX_COMPOSITE_DEPTH` the statement aborts. This
  is the **"bound at the producer"** rule §7 also follows: no over-deep type ever enters the catalog,
  so every later derived walk is transitively stack-safe with zero per-walk guards.
- **On-disk load (defense-in-depth) → `XX001` `data_corrupted`.** A conformant engine never *writes*
  an over-deep type (creation rejects it), so an over-deep chain in a file means tampering/corruption
  — treated like the existing dangling/cyclic-reference `XX001`. The two-pass load's acyclicity DFS
  is **extended into one pass that enforces acyclicity *and* depth**: it tracks a memoized absolute
  depth per type plus a descent guard. Both are needed — the descent guard (`levels_above >= MAX`)
  bounds the native recursion so the DFS itself cannot overflow on a deep tampered chain, while the
  post-compute `depth > MAX` value check catches an over-deep type reached through a memoized
  (already-`done`) shortcut, which the descent guard alone would miss when the catalog is colored
  bottom-up. The check runs **before** any store is built, so the subsequent `resolve_col_type` (and
  every later codec/comparator) walk is over a depth-bounded catalog.

**Why `32`, and why a fixed number.** Like `MAX_EXPR_DEPTH` (§7) it is a deterministic, cross-core
constant — the same `CREATE TYPE` is accepted or rejected with `54001` in Rust, Go, and TS alike (§8)
— rather than PG's runtime stack probe (PostgreSQL bounds composite nesting only by `check_stack_depth`,
a build/platform-dependent `54001`). `32` sits far above any real schema (composites nest a handful
deep in practice) yet well under the weakest core's native-stack limit for the derived codec/`record_in`
recursion, whose per-level frames are heavier than a parser level — so a tighter cap than §7's `256`
is the deliberate, defensible choice. It is a documented divergence from PG (the *overriding reason*:
cross-core determinism + the weakest core's stack, CLAUDE.md §1/§8/§13).

**Conformance.** [../conformance/suites/resource/composite_depth.test](../conformance/suites/resource/composite_depth.test)
pins the boundary cross-core: the chain `c1`…`c32` is accepted (depth 32 is declarable as a column,
exercising `resolve_col_type` 32 levels deep), `c33` aborts `54001`, depth is **max over fields not
sum** (two depth-31 fields are still depth 32), and an **array field counts as its element** (a
`c32[]` field is depth 33, rejected). Gated by `resource.composite_depth_limit`; jed-specific, so
**not** oracle-checked.

## 7c. Regex compiled-program size limit (landed)

The **third** structural-complexity trigger of `54001` ([regex.md](regex.md) §6). A regular
expression compiles to a flat NFA bytecode program (§ "`regex_compile` / `regex_step`" above), and
**bounded repetition `{n,m}` is unrolled** at compile (regex.md §3.3) — so a small *pattern* can
describe a very large *program* (`(a{1000}){1000}` ≈ 10⁶ instructions). The `regex_compile` cost
unit bounds this on a cost-limited handle (the ceiling aborts `54P01`), but the **unlimited** handle
(`max_cost = 0`, the trusted path) has no ceiling — so, exactly like `MAX_EXPR_DEPTH` (§7) and
`MAX_COMPOSITE_DEPTH` (§7b), a fixed **`MAX_REGEX_PROGRAM = 32768`** instruction cap is enforced at
the producer (the pattern compiler), aborting `54001` *projectively* — before the oversized program
is allocated, by checking the projected size of each `{n,m}` unroll rather than overrunning memory
and counting after. `2201B` (`invalid_regular_expression`) remains for a *malformed* pattern;
`54001` is for a *well-formed but too-large* one. `32768` sits far above any real pattern yet bounds
both the native allocation and the per-input-position O(|program|) match work. A deterministic,
cross-core constant (§8) — pinned in `impl/go/spec_constants_test.go` and the
[../conformance/suites/resource/regex_program_limit.test](../conformance/suites/resource/regex_program_limit.test)
boundary entry (gated by `resource.regex_program_limit`; jed-specific, **not** oracle-checked).
