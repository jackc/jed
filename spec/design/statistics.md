# Column statistics and `ANALYZE` — design

> P9 of [estimator.md](estimator.md): deterministic, transactional, persisted column-distribution
> facts collected through SQL and consumed by the cross-core physical-plan contract. Mechanical
> limits live in [../cost/estimator.toml](../cost/estimator.toml); runtime collection cost lives in
> [../cost/schedule.toml](../cost/schedule.toml); the v29 bytes live in
> [../fileformat/format.md](../fileformat/format.md).

## 1. Surface and PostgreSQL relationship

P9 implements one table per statement:

```sql
ANALYZE table_name
ANALYZE table_name (column_name [, ...])
```

The target uses ordinary qualified relation resolution: unqualified/main, session-local temporary,
and one named attachment are legal. `ANALYZE t` collects every declared column in ordinal order;
the column-list form collects only the listed columns in authored order. A duplicate column is
`42701`, an unknown column `42703`, an unknown table/database `42P01`, and a built-in catalog
relation target `42809`. An empty column list is syntax error `42601`.

This is a deliberately selected subset of PostgreSQL `ANALYZE`: database-wide and multi-target
forms, `VERBOSE`, `SKIP_LOCKED`, `BUFFER_USAGE_LIMIT`, inheritance/partition modifiers, extended
multi-column statistics, `ALTER COLUMN SET STATISTICS`, and automatic/background analyze are
deferred. No notice/progress rows are emitted. Success is a statement outcome with zero affected
rows.

`ANALYZE` is a transactional write. It is rejected `25006` by a read-only transaction/file, joins
the current explicit transaction when one is open, and otherwise commits atomically. It requires
both `SELECT` on the target user table and `allow_ddl`; this slice does not add PostgreSQL's separate
`MAINTAIN` privilege. A cost or decoding error rolls back every partially collected column fact.

## 2. Snapshot state and staleness

Each visible database snapshot owns an optional `ColumnStatistics` fact per `(table, column
ordinal)`. It is snapshot state beside the table/store maps: a writer clone shares it until changed,
commit publishes it, rollback discards the working replacement, and a reader keeps the facts pinned
with its data snapshot. Temporary-domain facts use the same in-memory shape but are never persisted.

Each fact records the population at collection:

```text
ColumnStatistics {
    analyzed_rows       non-negative i64
    stale               boolean
    null_count          non-negative i64, <= analyzed_rows
    width_sum           saturated non-negative i64
    distinct_count      optional non-negative i64
    sample_rows         non-negative u32, <= min(analyzed_rows, 30_000)
    sample_nonnull_rows non-negative u32, <= sample_rows
    mcv                 <= 100 (value, sample_frequency) entries
    histogram           <= 101 non-NULL bound values
}
```

`width_sum` is the sum of canonical comparison-key byte lengths for distribution-eligible values
and ordinary canonical value-body lengths otherwise. NULL contributes zero. `distinct_count` is the
analyzed non-NULL NDV estimate and is absent for a distribution-ineligible type.

`ANALYZE t(cols)` replaces those columns' facts and marks each replacement fresh. Existing facts for
unlisted columns stay untouched. A successful INSERT/UPDATE/DELETE against a table with any facts
retains all facts but marks them stale; the mark is transactional and persisted. Conservative
marking after a successful statement that affected zero rows is legal and canonical: every current
top-level DML target is marked stale once on successful completion. ANALYZE itself advances the
relation's existing estimator-revision token once, so a committed prepared plan cannot retain a
pre-ANALYZE plan. Ordinary DML already advances that token and therefore replans against current row
count plus retained stale facts.

Schema actions follow ordinal/type identity:

- DROP TABLE removes its facts; table rename moves them unchanged.
- A pure table/column rename retains facts.
- ADD COLUMN creates no fact for the new column and retains old ordinals.
- DROP COLUMN or any type/value-rewriting ALTER clears all facts for that table; index/constraint
  changes retain them.
- A collation version-skew makes an affected distribution fact unusable. Opening an older file
  still validates its typed value bytes and structural counts, but cannot revalidate old key order
  with the newly loaded collation bundle; the inert fact remains durable and hidden from planning.
  `upgrade_collations` clears affected facts while rebuilding keys, after which explicit ANALYZE may
  collect replacements. A collation-changing type/value rewrite clears the table's facts directly.

There is no clock age, DML threshold, or automatic refresh. Stale facts may become inaccurate—the
same operational trade PostgreSQL accepts between analyze runs—but remain deterministic.

## 3. Mechanical limits

The canonical constants are:

| fact | value |
|---|---:|
| statistics target | 100 |
| maximum sampled rows per column | 30,000 (`300 × target`, PostgreSQL's default shape) |
| KMV retained distinct hashes | 4,096 |
| MCV entries | 100 |
| histogram bounds | 101 |
| maximum retained value body and comparison key | 128 bytes each |
| proportional-NDV threshold | strictly greater than `1/10` of analyzed non-NULL rows |

These are spec data, not handle settings. In particular `work_mem` cannot change persisted facts or
selected plans. Collection processes columns sequentially, so resident collection memory is bounded
by one column's fixed sample/KMV limits independent of table width.

## 4. Deterministic collection

For every requested column, scan the complete table in ascending raw storage-key order. The scan is
an execution operation, not planning I/O: it faults ordinary pages and decodes the selected column
through the storage seam. It charges the same `page_read`, `storage_row_read`, overflow-page, and
`value_decompress` units as an ordinary one-column full scan.

Every row also charges `statistics_value × max(1, canonical_width)` before hashing or sample work.
For a distribution-eligible value, `canonical_width` is its order-preserving comparison-key byte
length under the column's effective collation. For an ineligible value it is its ordinary canonical
value-body length. NULL uses one. This new unit bounds full-population hashing/value inspection;
sample sorting is absolutely bounded by 30,000 retained entries.

For each row:

1. increment the exact NULL count, or add the non-NULL canonical width to `width_sum` with i64
   saturation;
2. if the type is distribution-eligible, hash its canonical comparison key into the KMV state;
3. compute the row-sample priority `FNV-1a-64(storage_key)` and retain the lowest 30,000
   `(priority, storage_ordinal)` pairs. `storage_ordinal` is the zero-based position in this scan and
   breaks hash collisions. A retained non-NULL value carries its ordinary value bytes and comparison
   key only when both are at most 128 bytes; otherwise it is an `oversized` sample marker.

The bounded sample is therefore a pure function of the table's storage-key/value set, not scan
timing, host randomness, map order, or the entropy seam. If the table has at most 30,000 rows, it is
the complete population.

### 4.1 Distribution eligibility and canonical equality

NULL/width facts are collected for every current `Type`. NDV/MCV/histogram facts initially require
the column type to have jed's canonical order-preserving key encoding: all key-encodable scalar
types and `range`. The effective text collation participates exactly as it does in comparison/index
encoding. `composite`, `array`, `json`, and `jsonb` are initially distribution-ineligible; their
NULL/width facts remain useful and `distinct_count` is NULL in `jed_statistics`.

Canonical comparison-key byte equality defines sample grouping and NDV hashing. This collapses
representations that database equality collapses (for example float signed zero/canonical NaN) and
uses collation equality for text. MCV/histogram values persist through the ordinary typed value
codec, but their comparison keys are regenerated and validated on load.

### 4.2 KMV NDV

FNV-1a-64 hashes each non-NULL canonical comparison key. Retain the 4,096 numerically smallest
distinct hashes in ascending order. Equal hashes are one KMV item; a hash collision can affect only
estimate quality, never SQL results, and is deterministic.

- If no more than 4,096 distinct hashes were seen, `distinct_count` is that exact hash count.
- Otherwise let `R` be the largest retained hash and `K = 4096`:

```text
ndv = ceil((K - 1) × 2^64 / (R + 1))
ndv = clamp(ndv, K + 1, analyzed_nonnull_rows)
```

The multiply/divide uses an exact 128-bit-or-wider temporary (Rust `u128`, Go standard-library
integer arithmetic, TypeScript `bigint`) and ceiling division. The result is stored as i64.

### 4.3 MCV list

Sort retained, non-NULL, non-oversized sample values by canonical comparison key and group equal
keys. Let `sample_nonnull_rows` include oversized non-NULL markers.

If the sample is the complete table population, contains no oversized non-NULL value, and has at
most 100 groups, every group becomes an MCV entry. Otherwise a group is eligible only when:

```text
sample_frequency >= 2
sample_frequency × analyzed_distinct_count > sample_nonnull_rows
```

That is, it appeared repeatedly and is more common than the analyzed average. Sort eligible groups
by descending sample frequency, then ascending canonical key; retain the first 100. Persist values
in that order with their exact sample frequencies. MCV values are distinct.

### 4.4 Equi-depth histogram

If any sampled non-NULL value is oversized, omit the histogram rather than bias its ordering.
Otherwise remove every sampled row belonging to a selected MCV group, sort the remaining values by
canonical key (duplicates retained), and let the resulting length be `H`.

If `H < 2`, omit the histogram. Otherwise choose `B = min(101, H)` bounds in ordinal order:

```text
rank(i) = floor(i × (H - 1) / (B - 1)), for i in 0...(B - 1)
bound(i) = remaining[rank(i)]
```

Duplicate adjacent bounds are retained: they carry sampled mass and make step-CDF estimates reflect
minor duplicates that were not MCVs. All arithmetic is integer.

## 5. Applying fresh or stale facts

Let `N` be the current exact table row count. If `analyzed_rows == 0`, distribution fractions are
unusable for a later non-empty table and the existing no-statistics fallback applies. Otherwise:

```text
current_null    = scale_ceil(N, null_count / analyzed_rows)
current_null    = min(current_null, N)
current_nonnull = N - current_null
```

Every MCV frequency scales as `scale_ceil(N, sample_frequency / sample_rows)`, folded in stored MCV
order and capped so the cumulative MCV population never exceeds `current_nonnull`. Histogram mass is
the remaining non-NULL population.

NDV uses PostgreSQL's fixed/proportional convention over the analyzed non-NULL population `A`:

```text
if distinct_count × 10 > A:
    current_ndv = scale_ceil(current_nonnull, distinct_count / A)
else:
    current_ndv = distinct_count
current_ndv = min(current_ndv, current_nonnull)
```

Thus an enum-like five-value column stays near five while a key-like column grows/shrinks with the
table. `stale` does not change these formulas; it prevents absence observations from becoming
structural proofs. A fresh complete-population MCV list may estimate an absent literal as zero. A
stale or sampled list uses at least one row for a positive residual population.

Average width is `ceil(width_sum / analyzed_nonnull_rows)` and does not row-count-scale. It replaces
`default_variable_key_bytes` where the estimator prices variable-width hash keys.

## 6. Selectivity rules

Facts apply only when the resolved operand is a bare column of the owning base relation and the
literal is plan-time known/coercible to that exact column type. Otherwise §7's structural fallback
remains authoritative.

- `IS NULL` / `IS NOT NULL`: `current_null` and its complement.
- Literal equality: a matching MCV's scaled rows; otherwise divide non-MCV non-NULL rows by
  `max(1, current_ndv - mcv_count)`. A fresh complete MCV may prove zero; stale/sample facts do not.
- Generic parameter equality: `ceil(current_nonnull / max(1, current_ndv))`; it is statistics-aware
  but never bucket/value-aware.
- `<>`, NOT, and NOT IN complements subtract from `current_nonnull`, not `N`, preserving SQL 3VL.
- `IN`: de-duplicate known literals canonically, fold equality estimates in authored order, and cap
  at `current_nonnull`. A known NULL contributes no TRUE rows.
- Inequality: add scaled MCV rows satisfying the operator. For histogram residual mass, find the
  lower-bound (`<`) or upper-bound (`<=`) insertion ordinal among the stored bounds and scale by
  `ordinal / (bound_count - 1)`, capped to residual mass. `>`/`>=` are the non-NULL complements.
  There is intentionally no type-specific within-bucket interpolation.
- A paired range/BETWEEN computes the upper step CDF minus the lower step CDF once, plus MCVs inside
  the interval; it does not multiply two independent inequalities.
- Bare boolean uses its TRUE MCV when present, otherwise NDV average; pattern/regex and opaque
  predicates retain shared defaults.

AND still folds source order; OR remains the deterministic disjoint-union upper bound. Statistics
replace individual leaf estimates and exact NULL complements but do not introduce multi-column
correlation or OR-overlap models.

An equality join between two bare distribution-eligible columns with facts on both sides uses:

```text
ceil(left_nonnull × right_nonnull / max(left_ndv, right_ndv))
```

with saturation. MCV-to-MCV join skew is deferred. GROUP BY/DISTINCT over bare columns uses the
product of each column's `current_ndv + (current_null > 0 ? 1 : 0)`, capped by input rows; any other
expression keeps `default_distinct_values`. Multi-column dependency statistics are deferred.

## 7. Persistence — format version 29

Persistent statistics are ordinary kind-tagged catalog entries after the table-entry group. A
single catalog entry must still fit one page. `entry_kind = 4` has a `stats_kind` subtag:

- `0` summary: table name, column ordinal, flags (`stale`, distribution eligibility), analyzed/null/
  width/NDV counts, sample counts, and declared MCV/histogram entry counts;
- `1` MCV: table name, column ordinal, MCV ordinal, sample frequency, and one typed non-NULL value;
- `2` histogram: table name, column ordinal, bound ordinal, and one typed non-NULL value.

The 128-byte retained-value limit plus 63-byte identifier limit guarantees each value entry fits the
minimum 256-byte page payload. Entries sort by `(lowercase table name, column ordinal, stats_kind,
ordinal)`. On load, summaries must be unique; referenced tables/ordinals must exist; counts and
ordinals must be exact/contiguous; MCV values must be distinct and in canonical frequency/key order;
histogram bounds must be nondecreasing; every value must decode under the column type and satisfy
the size/type gates. Any violation is `XX001`.

Files without statistics still move to v29 by the exact-version clean break (version bytes/meta
CRC). No new page type, meta root, value codec, table B+tree, or planning-time leaf read is added.

## 8. Introspection

The read-only computed relation follows [introspection.md](introspection.md)'s ordinary model:

```text
jed_statistics(
    table_name       text NOT NULL,
    column_name      text NOT NULL,
    analyzed_rows    i64 NOT NULL,
    is_stale         boolean NOT NULL,
    null_count       i64 NOT NULL,
    distinct_count   i64,
    sample_rows      i64 NOT NULL,
    average_width    i64,
    mcv_count        i32 NOT NULL,
    histogram_count  i32 NOT NULL
)
```

One row exists per collected column, ordered internally by lowercased table name then column ordinal.
It charges one `generated_row` at the source, no page/storage row units, is independently SELECT-
gated as `jed_statistics`, and scopes through `main`/`temp`/attachments like the other catalog
relations. Typed MCV/histogram arrays stay internal in P9.

## 9. Conformance obligations

- Shared SQL corpus: surface/errors, selective column replacement, stale retention/scaling,
  transaction rollback, temp/attachment scope, `jed_statistics`, EXPLAIN plan/estimate flips at
  equal row counts, actual costs, and NoREC equivalence.
- Per-core tests: exact collection/KMV/MCV/histogram vectors, prepared-cache fresh-vs-hit parity,
  corruption rejection, host privilege/read-only classification, and statement rollback under a
  cost ceiling.
- File format: v29 golden isolating fresh and stale stats plus cross-core/Ruby byte identity.
- Benchmarks: uniform and skewed distributions before/after ANALYZE with identical checksums.

PostgreSQL is the SQL-result/surface oracle for the supported command form. The deterministic sample,
integer estimator, `jed_statistics`, persisted stale bit, and plan choices are jed-owned contracts.
