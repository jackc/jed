# Conformance test-gap remediation plan

> A worklist of missing edge-case tests found by auditing the 122-file corpus against the
> design specs, the capability manifest, and the three core implementations (2026-06-17).
> Every item below is for a feature that is **confirmed implemented** — these are coverage
> holes, not deferred features. Resolve items top-down: P0 → P1 → P2.

## How to resolve an item

Each item names a target `.test` file and the records to add. For every item:

1. **Add the records** in the existing file's style (blank-line-separated; `statement ok` /
   `statement error <sqlstate>` / `query <coltypes> <sortmode>` + `----` + expected rows).
2. **Sort discipline** — a multi-row query carries an order-determining `ORDER BY` + `nosort`,
   **or** uses `rowsort`. Never pin row order without `ORDER BY`.
3. **Update the file's `# requires:` header** if a record needs a capability not already
   declared (check `manifest.toml`).
4. **Oracle-check the PG-comparable rows**: `rake corpus:check[<file>]`. If jed deliberately
   diverges, register the divergence in `oracle_overrides.toml` with a reason (`corpus:check`
   warns on an unregistered divergence). Items flagged **[divergence]** below are expected to
   need a ledger entry.
5. **Cost/type/name assertions are hand-authored** — the oracle cannot derive `# cost:`,
   `# types:`, or `# names:`. Items flagged **[cost]** / **[types]** require these by hand.
6. **Run all three cores' harnesses** (Rust, Go, TS) — the corpus is the cross-core contract;
   a record passes only when all three agree.
7. Items flagged **[verify]** depend on an exact value/code you must confirm against the impl
   or `spec/errors/registry.toml` *before* writing the expected output — do not guess.

Confirmed codes used below: overflow `22003`, div/zero `22012`, datetime field overflow
`22008`, cardinality `21000`, depth limit `54001`, cost ceiling `54P01`, invalid text rep
`22P02`, array dim/subscript `2202E`, not-null `23502`, unique `23505`, check `23514`,
type mismatch `42804`, ambiguous column `42702`, syntax/misuse `42601`.

No item here adds a query optimization, so **no NoREC/TLP relation is required**. No item
changes a user-facing SQL feature (these test already-shipped surface), so **no `/web` update
is required** — except G1, which gives the float-math functions their first documented contract;
if the `/web` SQL reference omits them, add them there in the same change.

---

## Batch 1 status — LANDED 2026-06-17 (G1–G4)

All four pass on **all three cores** (Rust, Go, TS — `123 passed, 0 failed`), and `rake verify`
is green. Empirically derived via the Rust CLI (`cli/target/debug/jed -c`) and cross-checked on
every core's harness.

- **G1** — NEW [suites/expr/float_math.test](suites/expr/float_math.test). All 11 float-math
  functions, gated under `types.float64`. jed confirmed: `round` is half-AWAY (2.5→3); domain
  errors all trap `22003` (jed has no PG `2201E`/`2201F` — engine-wide collapse to `22003`);
  arg resolution is `none` (no implicit int/decimal→float — `sqrt(4)` → `42883`). 3 override
  ledger entries added (`sqrt(4)`, `sqrt(4.0)`, the `round(2.5)` deterministic-half-away tie).
- **G2** — [suites/aggregates/sum.test](suites/aggregates/sum.test): added `SUM(int32)` widening
  past int32-max with `# types: int64`. The overflow *trap* is documented as unreachable with
  practical data (int64 accumulator needs ~4.3e9 rows; decimal cap needs a ~131072-digit value).
- **G3** — [suites/expr/arithmetic.test](suites/expr/arithmetic.test): `INT_MIN / -1` traps
  `22003` for all three widths (matches PG); `% -1` → `0` for **all three widths** (matches PG),
  after the fix below.
- **G4** — [suites/null/null_basics.test](suites/null/null_basics.test): NULL propagation through
  `+ * - / %`, the UNKNOWN-predicate filter, and a mixed projection.

### Findings surfaced during Batch 1

1. **`int64 % -1` trapped `22003` (was a bug) — FIXED.** All three cores trapped it, but PG
   returns `0` and jed *itself* returned `0` for `int16/int32 % -1`. The result (0) is in range —
   the trap was a hardware-IDIV artifact of computing in 64-bit, with no determinism benefit.
   **Fixed** (decision: match PG): each core now special-cases divisor `-1` in modulo to yield `0`
   (Rust [executor.rs](../../impl/rust/src/executor.rs) `eval_arith`, Go
   [executor.go](../../impl/go/executor.go) `evalArith`, TS
   [executor.ts](../../impl/ts/src/executor.ts) `evalArith`); the division-by-`-1` overflow trap is
   untouched. Documented in [types.md §3](../design/types.md) and pinned for all three widths in
   arithmetic.test. Verified by `rake ci` (3 cores + NoREC sweep).
2. **float `round` half-away (intentional).** Registered as an intentional divergence (deterministic
   engine-wide rounding vs PG's platform `rint`). Flag if you disagree.

### Oracle-check — DONE (all four files regenerate byte-identically from PG)

`rake corpus:check` run against the live PostgreSQL service for all four files; all now clean.
- `arithmetic.test`, `sum.test`, `null_basics.test`: clean with **no** new overrides — jed matches
  PG exactly. This independently confirms the `int64 % -1 → 0` fix matches PG (and that `INT_MIN /
  -1` traps as PG does), and that `SUM(int32)` widening and NULL-arithmetic match PG.
- `float_math.test`: 4 additional overrides registered, exactly as predicted:
  - `round(x, 1)` — jed extension; PG's 2-arg round is numeric-only (no `round(double precision, integer)`).
  - `sqrt(-1.5)` → jed `22003` vs PG `2201F`; `ln(0)` / `log10(0)` → jed `22003` vs PG `2201E`
    (jed's registry has only `22003` for numeric range/domain — the same blanket collapse float.test uses).
  - `exp`/`pow` overflow → `22003` matched PG (no override). The `sqrt(4)`/`sqrt(4.0)` strict-typing
    and `round(2.5)` half-away overrides (added earlier) were confirmed in use.

The ledger now holds **61** overrides (was 54 pre-Batch-1: +3 earlier, +4 here).

---

## P0 — verified, highest impact

### G1 — Float math-function family has **zero** tests  → `expr/scalar_functions.test`
11 catalog functions implemented in Rust + Go with **no corpus invocation at all**:
`ceil, floor, trunc, round/1, round/2, sqrt, exp, ln, log10, pow, sin, cos, tan`.
- [ ] Happy-path values (R-tag, tolerant compare): `ceil(1.5::float64)`→2, `floor(-1.5)`→-2,
      `trunc(-1.7)`→-1, `sqrt(4)`→2, `sqrt(2)`≈1.414…, `exp(0)`→1, `ln(1)`→0, `log10(100)`→2,
      `pow(2,10)`→1024, `sin(0)`/`cos(0)`/`tan(0)`.
- [ ] Error paths (all `22003` per catalog): `sqrt(-1::float64)`, `ln(0)`, `ln(-1)`,
      `log10(0)`, `exp(710)` (overflow), `pow(2, 1e6)` (overflow).
- [ ] `round/2` and float rounding mode — **[verify]** float `round` half-rule against PG +
      the impl (binary `round` may differ from decimal half-away; confirm before pinning).
- [ ] **[cost]** add `# cost:` on a representative call (one `operator_eval` each).
- Oracle-check against the live PG service (all PG-comparable).

### G2 — `SUM` overflow trap + `SUM(int32)`  → `aggregates/sum.test`
Only widening (non-trapping) cases exist; `aggregates.md` §9 mandates a trapping value too,
and `SUM(int32)` is absent entirely.
- [ ] `SUM(int32)` column → `int64` result; include the "many int32 exceeding int32 but
      fitting int64 → no trap" mid-range case. **[types]** assert `int64`.
- [ ] **[verify]** The trapping value. Read `executor.rs` (~6154–6365) + `aggregates.md` §9 to
      find which accumulation actually overflows (int64 accumulator vs decimal cap) and pin a
      reachable `22003`. If the int64-accumulator overflow needs an infeasible row count,
      document that and test the genuinely reachable trap instead.

### G3 — `INT_MIN / -1` division overflow  → `expr/arithmetic.test`
The one integer op besides negation that overflows; a hardware trap on most CPUs.
- [ ] int64 `-9223372036854775808 / -1` → `22003`; int32 `-2147483648 / -1` → `22003`;
      int16 `-32768 / -1` → `22003`. (Stage values via INSERT into typed columns — the bare
      literal cannot be written directly.)
- [ ] Contrast: `INT_MIN % -1` → `0` (no trap; defined). Confirm against PG.

### G4 — NULL propagation through arithmetic  → `null/null_basics.test`
`expr/arithmetic.test` has **zero** NULL cases.
- [ ] `NULL + 1`, `NULL * 2`, `-NULL`, `NULL / 1`, `NULL % 2` all → `NULL` (projected).
- [ ] `WHERE v + 1 = 11` excludes the NULL-`v` row (NULL predicate → not selected).
- [ ] rowsort; use a column holding a NULL.

---

## P1 — NULL/3VL traps & cross-core determinism

### G5 — `NOT IN` with NULL LHS / empty / NULL-in-set  → `subquery/in.test`
Table currently has no NULL key; the spec's named corner is unhit.
- [ ] Add a row with `k = NULL`. `k NOT IN (SELECT … empty)` → TRUE (row survives, even NULL `k`).
- [ ] `k NOT IN (SELECT v …)` where the set contains a NULL → UNKNOWN → no rows.
- [ ] `k IN (SELECT … empty)` → FALSE. Oracle-check.

### G6 — Array comparison `<` / `<>` / ORDER BY with NULL **elements**  → `types/array.test`
Only `=` is tested. Arrays use btree semantics (definite boolean), **not** composite 3VL.
- [ ] `ARRAY[1,NULL] < ARRAY[1,2]`, `ARRAY[1,NULL,3] <> ARRAY[1,2,3]`, ORDER BY over
      NULL-element arrays — all yield **definite** booleans. Use `int32[]` columns/literals.

### G7 — `# types:` on the promotion tower + cast targets  → `compare/promotion.test`, `cast/narrowing.test`  **[types]**
All int widths render `I`, so a wrong resolved width is invisible without `# types:`.
- [ ] `a + c` (int16 + int64) → `# types: int64`; `CAST(x AS int16)` → `# types: int16`;
      `int + decimal` → `# types: decimal`.

### G8 — `array_out` / `array_in` text-I/O quoting  → `types/array.test`
Byte-identity surface; only comma-quoting is tested.
- [ ] `array_out` of `text[]` with: empty-string element, the literal token `NULL` (must quote
      → `"NULL"`), backslash, `{`/`}`, double-quote.
- [ ] `array_in`: `'{a,"NULL",c}'::text[]` (quoted = 4-char string ≠ NULL element),
      `'{ 1 , 2 }'::int32[]` (whitespace), escapes, unquoted case-insensitive `nUlL` → NULL.

### G9 — `text[]` ordering / comparison (UTF-8 collation recursion)  → `types/array.test`
Every array comparison test uses `int32[]`; the text recursion is a known cross-core hotspot.
- [ ] ORDER BY over a `text[]` column with multibyte / mixed-length elements; `WHERE tags = ARRAY['a','b']`.

### G10 — Decimal exact-half rounding + negative-zero  → `types/decimal.test`
The money rows never hit the spec's canonical half example.
- [ ] Store `0.125` into `numeric(p,2)` → `0.13`; `-0.125` → `-0.13`.
- [ ] `CAST(-0.4 AS int32)` → `0` (not `-0`); `-0.0` literal canonicalizes to `0`.

### G11 — Float `-0` render / NaN canonicalization / float32 inexact  → `types/float.test`
- [ ] `SELECT float64 '-0'` renders `-0`; distinct NaN inputs canonicalize identically on store.
- [ ] `0.1::float32` (binary32 rounding) and the lossy `float64→float32` explicit cast.

---

## P2 — boundaries, joins, safety ceilings, and the tail

### G12 — Empty *scanned* table → ORDER BY / LIMIT / DISTINCT  → `query/order_by.test`, `query/distinct.test`
- [ ] Empty table: `ORDER BY a` → 0 rows; `LIMIT 5` → 0 rows; `SELECT DISTINCT a` → 0 rows.

### G13 — Exact int16 cast boundary  → `cast/narrowing.test`
- [ ] `CAST(32767 AS int16)` fits; `CAST(32768 AS int16)` → `22003`; `CAST(-32768 AS int16)`
      fits; `CAST(-32769 AS int16)` → `22003`.

### G14 — ON-vs-WHERE outer-join trap for RIGHT/FULL  → `joins/right.test`, `joins/full.test`
- [ ] RIGHT JOIN: same predicate on the nullable side in `ON` (preserves) vs `WHERE` (downgrades).
- [ ] FULL JOIN counterpart; plus a FULL join over **disjoint** keys (cardinality m+n, all NULL-extended).

### G15 — Correlated scalar subquery returning >1 row → `21000` per-row  → `subquery/correlated.test`
- [ ] Correlated scalar subquery returning 2 rows for some outer row → `21000` at execution
      (not plan time). Add a variant that only trips on a *later* outer row.

### G16 — SRF cost-ceiling abort (`54P01`)  → `query/generate_series.test`, `query/unnest.test`
The §13 untrusted-SRF guarantee is asserted in prose but never tripped.
- [ ] `# max_cost: <small>` over `generate_series(1, 1000000)` → `54P01`; same for a large `unnest`.

### G17 — Depth-limit exact boundary (256)  → `resource/depth_limit.test`
Tests use ~4 vs ~300; the exact `MAX_EXPR_DEPTH` edge is unbracketed.
- [ ] ~255-deep nesting succeeds; ~257-deep → `54001`. **[verify]** the exact constant in `cost.md` §7.

### G18 — Multi-row UPDATE/DELETE RETURNING  → `dml/returning.test`
Every RETURNING case affects ≤1 row.
- [ ] `UPDATE … WHERE id>=4 RETURNING id, old.v, new.v` → 2+ rows (rowsort).
- [ ] `DELETE … WHERE id>=4 RETURNING *` → 2+ rows.

### G19 — Expression precedence / composition  → `expr/precedence.test`, `expr/unary_minus.test`, `expr/case.test`, `expr/between.test`, `expr/in_list.test`, `expr/like.test`, `expr/boolean.test`
- [ ] Unary minus vs `*`/`/`: `-2 * 3` → `-6`; `-a * b` parses as `(-a)*b`.
- [ ] BETWEEN reversed bounds (`a BETWEEN 20 AND 10` → empty), `NOT BETWEEN`, expression bounds.
- [ ] Nested CASE; CASE as an arithmetic operand; simple `CASE NULL WHEN NULL THEN …` (NULL scrutinee never matches).
- [ ] `IS NULL` precedence (`a IS NULL AND b = 1`); non-associativity `x IS NULL IS NULL` → `42601`.
- [ ] LIKE: exact (no wildcard); backtracking `'ab' LIKE 'a%b%'`; `___` vs `__`; `\\` escaping the escape; `'%' LIKE '%'`.
- [ ] IN list cross-width promotion (`int64_col IN (int16_lit, int32_lit)`); single-NULL list `a IN (NULL)` → UNKNOWN.

### G20 — Array functions: NULL-safe matching + boundaries  → `expr/array_concat_search.test`, `expr/array_functions.test`
- [ ] NULL-safe (NOT DISTINCT FROM) edit/search over `{1,NULL,3}`: `array_remove(xs, NULL)`→`{1,3}`,
      `array_replace(xs, NULL, 99)`, `array_position(xs, NULL)`→2.
- [ ] `array_replace` shape-preserving on a 2-D array; `array_remove` on multidim → `0A000`.
- [ ] `array_cat(NULL::int32[], NULL::int32[])` → NULL; the N==M+1 dimension branch (1-D ∥ 2-D).
- [ ] `array_length(xs, 0)` / negative dim → NULL; `array_position(xs, v, <start past end>)` → NULL;
      `array_positions(xs, <absent>)` → `{}` (vs NULL array → NULL).
- [ ] `num_nulls(VARIADIC text[])`; all-NULL spread `num_nulls(NULL,NULL,NULL)` → 3.

### G21 — Multidim / lower-bound array edges  → `types/array_multidim.test`, `types/array_slice.test`
- [ ] `>6` dimensions → error (bracket `MAXDIM=6`): constructor → `2202E`, literal → `22P02`. **[verify]** codes.
- [ ] Negative/zero custom lower bound: `'[-2:0]={7,8,9}'::int32[]` — `g[-2]`=7, `array_lower`=-2, `array_dims`=`[-2:0]`, `array_out` round-trip.
- [ ] Upper-NULL slice `a[m:NULL]` → NULL (the lower-NULL side `a[NULL:n]` is already tested).

### G22 — GROUP BY over an array column  → `types/array.test`
- [ ] `GROUP BY xs` with duplicate arrays and a `{1,NULL,3}` group (NULL-element arrays group together).

### G23 — Composite NULL deciding-field ordering  → `types/composite.test`
- [ ] `ROW(1,NULL) < ROW(1,2)` → UNKNOWN (3VL — the deliberate contrast with the array case in G6). **[divergence]** vs array.

### G24 — Timestamp / interval edges  → `types/timestamp.test`, `types/interval.test`
- [ ] `'2024-01-01 24:00:00'` → next-day `00:00:00`; `'…24:00:01'` → `22008`.
- [ ] 7-digit fraction sub-µs rounding (half-away) — **[divergence]** vs PG; register in ledger.
- [ ] Near-sentinel finite timestamp boundary (largest finite vs over-range → `22008`).
- [ ] Interval mixed-sign render (`INTERVAL '-1 year 2 mons'` → `-1 years +2 mons`); field-overflow parse → `22008`.

### G25 — Set-op trailing window + output naming  → `setops/intersect.test`, `setops/except.test`, `setops/types.test`
- [ ] INTERSECT/EXCEPT with trailing `ORDER BY … LIMIT/OFFSET` (only UNION has it today).
- [ ] Mixed-multiplicity chain: `… UNION ALL … UNION …` (does the trailing distinct-UNION dedup the accumulated multiset?).
- [ ] **[names]** left-operand computed column → `?column?`; two all-NULL columns unify to `text`.

### G26 — Cross-width join keys + non-equi / expression ON  → `joins/typed_keys.test`, `joins/inner.test`, `joins/self_join.test`
- [ ] `int16 ⋈ int64` join key (promotion tower at the join boundary).
- [ ] Non-equi ON (`a.k > b.k`); expression-keyed equi (`a.k = b.k + 1`).
- [ ] Self LEFT join (preserve the self-unmatched row, e.g. CEO with NULL manager).

### G27 — Subquery composed with a JOIN outer  → `subquery/correlated.test`
- [ ] EXISTS / scalar subquery correlated to a **joined** alias; correlated reference resolving
      across a multi-relation outer; ambiguous correlated name → `42702`.

### G28 — Mutation / constraint / DEFAULT / RETURNING corners  → `transactions/failed.test`, `ddl/check.test`, `ddl/column_default_expr.test`, `resource/cost_limit.test`
- [ ] Autocommit: a failing statement (e.g. dup-key `23505`) followed by a successful one runs
      normally (no poison — only explicit BEGIN blocks poison).
- [ ] UPDATE where one row violates NOT NULL (`23502`) and another CHECK (`23514`) → fixed per-row
      precedence fires first (currently asserted for INSERT only).
- [ ] DEFAULT **expression** overflow at INSERT: `x int16 DEFAULT 30000 * 2` → `22003` per row
      (CREATE-time only checks the result *type*).
- [ ] `# max_cost:` mid-`RETURNING` → `54P01` with the table left unwritten (all-or-nothing).

### G29 — Aggregate combined / grouped / ordered / decimal-cost corners  → `aggregates/whole_table.test`, `aggregates/group_by.test`, `aggregates/sum.test`, `aggregates/avg.test`
- [ ] `SELECT COUNT(*), SUM(a), AVG(a), MIN(a), MAX(a) FROM empty` → `0 NULL NULL NULL NULL`
      (one row); same over an all-NULL column (the COUNT=0-while-rest-NULL contrast in one row).
- [ ] Grouped `COUNT(*)` vs `COUNT(a)` with NULL `a` in some groups (`COUNT(col) < COUNT(*)` per group).
- [ ] `GROUP BY g ORDER BY g DESC` and `… LIMIT 2` (post-aggregation window; NULLs-last under DESC).
- [ ] `MIN/MAX(decimal)` value-canonical (`1.5` vs `1.50`); **[cost]** SUM/AVG over large-coefficient
      decimals asserting non-zero `decimal_work`.

### G30 — Low-severity tail
- [ ] `IS NULL` / `IS NOT NULL` across every scalar type in one consolidated record set  → `null/null_basics.test`.
- [ ] bytea embedded-NUL ordering (`\x0061` vs `\x61`); uuid off-variant 32-hex stores as raw 16 bytes  → `types/bytea.test`, `types/uuid.test`.
- [ ] Pin the **rejection** of deferred forms: `SELECT unnest(ARRAY[1,2])` (SELECT-list SRF) and
      `WITH ORDINALITY` → defined error code (so an accidental accept is caught)  → `query/unnest.test`. **[verify]** code.
- [ ] `SELECT DISTINCT v … LIMIT 1` with no ORDER BY (windows the distinct set; use an all-equal column so it's rowsort-deterministic)  → `query/distinct.test`.
- [ ] `generate_series(1, 5, -1)` → empty; `generate_series(5, 1, 1)` → empty (explicit-step direction-mismatch)  → `query/generate_series.test`.

---

## Suggested sequencing

| Batch | Items | Rationale |
|---|---|---|
| 1 | G1–G4 | Verified, highest impact; G1/G3 close real safety/crash exposure. |
| 2 | G5–G11 | NULL/3VL traps + the cross-core determinism assertions (`# types:`, text I/O, rounding). |
| 3 | G12–G18 | Boundaries, outer-join semantics, and the resource ceilings actually firing. |
| 4 | G19–G30 | Breadth: expression composition, array surface, mutation/aggregate corners, tail. |

Each item is independently committable as a vertical slice ("make these records pass on all
three cores"). Mark an item's checkbox when its records pass `rake ci` and any divergence is
registered in `oracle_overrides.toml`.
