# Column statistics and `ANALYZE` — design

> Deterministic, transactional, persisted column-distribution facts collected through SQL and
> consumed by the cross-core physical-plan contract in [estimator.md](estimator.md). Mechanical
> limits live in [../cost/estimator.toml](../cost/estimator.toml); runtime collection cost lives in
> [../cost/schedule.toml](../cost/schedule.toml); the v29 bytes (and the v42 GIN term facts) live in
> [../fileformat/format.md](../fileformat/format.md).

## 1. Surface and PostgreSQL relationship

The implemented surface accepts one table per statement:

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
    terms               present only for a term-eligible column (§4.5):
        term_occurrences    saturated non-negative i64
        untracked_term_rows non-negative i64
        entries             <= 100 (term bytes, row_count) entries
}
```

`width_sum` is the sum of canonical comparison-key byte lengths for distribution-eligible values
and ordinary canonical value-body lengths otherwise. NULL contributes zero. `distinct_count` is the
analyzed non-NULL NDV estimate and is absent for a distribution-ineligible type.

`ANALYZE t(cols)` replaces those columns' facts and marks each replacement fresh. Existing facts for
unlisted columns stay untouched. A successful INSERT/UPDATE/DELETE against a table with any facts
retains all facts but marks them stale when it changes rows; the mark is transactional and persisted.
A zero-row disposition preserves the facts and estimator revision. Sequence or other-domain effects
still commit independently (transactions.md §2.1). Each changed target is marked once per statement.
ANALYZE itself advances the
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
| most-common GIN terms | 100 |
| term lossy-counting bucket width | 10,000 term occurrences |
| maximum retained term | 128 bytes (the retained-value limit) |

These are spec data, not handle settings. In particular `work_mem` cannot change persisted facts or
selected plans. Collection processes columns sequentially, so resident collection memory is bounded
by one column's fixed sample/KMV limits independent of table width, plus the term counter of §4.5,
whose size the lossy-counting bound limits to `O(w · log(L / w))` entries for `L` term occurrences
and bucket width `w`.

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

4. if the column is term-eligible (§4.5), extract the row's GIN term set and feed it to the term
   counter, charging `statistics_value × max(1, term_bytes)` per term. A NULL row has no terms.

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

### 4.5 GIN term frequencies

A column is **term-eligible** when it has a GIN operator class ([gin.md §3](gin.md)): a `jsonb`
column (`jsonb_ops`), or an array whose element type a GIN index admits (`array_ops`). Eligibility
follows the column type alone; an index need not exist, because the facts also refine the logical
estimate of the predicates the operator class serves (§6). This is PostgreSQL's
`most_common_elems` (collected for every array column by `array_typanalyze.c`) generalized to every
GIN term, including `jsonb_ops`' key and value terms, which PostgreSQL does not collect.

A row's **term set** is exactly the set its GIN entries carry ([gin.md §4](gin.md)): the distinct
element terms of an array, or the distinct `jsonb_ops` terms of a document, as byte strings in
ascending order. An empty array, `{}`, `[]`, and a NULL value have none. Term bytes never depend
on a collation, so a collation change never affects these facts. Let `L` be the total number of
term occurrences — the sum of the rows' term-set sizes over the whole table, saturated at i64.
`term_occurrences` stores `L`; it estimates the GIN index's entry count.

Counting is PostgreSQL's lossy counting (Manku–Motwani) over the **full population** in the scan
order of §4 — rows in ascending storage-key order, each row's terms in ascending byte order —
with bucket width `w = 10,000`. Number the occurrences `1, 2, …`; occurrence `p` lies in bucket
`b = ceil(p / w)`. Keep a table `term → (count, delta)`:

1. A term longer than 128 bytes is **oversized**: it advances `p` but is not tracked.
2. Otherwise, if the term is present, `count += 1`; else insert it with `(1, b - 1)`.
3. After occurrence `p` with `p mod w = 0`, remove every entry with `count + delta <= p / w`. If
   any entry was ever removed, the counter is **pruned**.

Removal is a per-entry predicate, so the result is independent of the table's iteration order.
At the end, order the entries by descending `count`, then ascending term bytes; the first 100 are
the persisted **term list**, each with `row_count = count` (a lower bound on the rows containing
the term, short by at most `delta`). Then

```text
untracked_term_rows = max(
    count of the 101st entry, if there is one,
    ceil(L / w), if the counter was pruned,
    1, if any term was oversized
)   (0 when none applies)
```

estimates the row count of any term not on the list: a term removed from the table had a count of
at most `ceil(L / w)` when it was last removed, and a term past the 100th is counted no higher than
the 101st entry. `untracked_term_rows = 0` means the list is **complete**: every term in the table
is on it with its exact row count.

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

GIN term facts scale the same way, against `analyzed_rows` (a NULL row contains no term):

```text
term_rows(t)  = min(scale_ceil(N, row_count(t) / analyzed_rows), current_nonnull)  for a listed t
term_rows(t)  = 0                                       for an unlisted t, complete fresh list
term_rows(t)  = max(1, min(scale_ceil(N, ceil(untracked_term_rows / 2) / analyzed_rows),
                           current_nonnull))            for any other unlisted t
total_entries = scale_ceil(N, term_occurrences / analyzed_rows)
```

Half the untracked bound is PostgreSQL's `minfreq / 2` default for an element missing from
`most_common_elems`. Only a fresh complete list proves a term absent; a stale complete list gives an
unlisted term one row, like the MCV rule above.

## 6. Selectivity rules

Facts apply only when the resolved operand is a bare column of the owning base relation and the
literal is plan-time known/coercible to that exact column type. A bound parameter is plan-time known
in its execution's custom plan ([estimator.md §6.1](estimator.md)). Otherwise §7's structural
fallback remains authoritative.

- `IS NULL` / `IS NOT NULL`: `current_null` and its complement.
- Literal equality: a matching MCV's scaled rows; otherwise divide non-MCV non-NULL rows by
  `max(1, current_ndv - mcv_count)`. A fresh complete MCV may prove zero; stale/sample facts do not.
- Unbound parameter equality: `ceil(current_nonnull / max(1, current_ndv))`; it is statistics-aware
  but never bucket/value-aware.
- `<>`, NOT, and NOT IN complements subtract from `current_nonnull`, not `N`, preserving SQL 3VL.
- `IN`: de-duplicate known literals canonically, fold equality estimates in authored order, and cap
  at `current_nonnull`. A known NULL contributes no TRUE rows.
- `col = ANY(array)` with an element type exactly the column's, or a promoted integer/decimal one
  ([cost.md §3](cost.md) "OR / IN-list"): the `IN` rule over the array's estimator elements
  ([estimator.md §7](estimator.md)) — each distinct known element by statistics key adds its
  equality rows, a NULL element adds none, an element of unknown value adds the unbound-parameter
  equality rows, capped at `current_nonnull`. A promoted element is converted to the column's type
  first; one no column value can equal adds none.
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

### 6.1 GIN predicates

A GIN-served predicate over a bare term-eligible column with facts — the conjunct shapes a GIN
index accelerates ([gin.md §1/§2.1](gin.md): `col @> Q`, `col && Q`, `c = ANY(col)`, `col ? k`,
`col ?| K`, `col ?& K`, and `jsonb` `col @> Q` / `Q <@ col`) — whose query operand is a plan-time
literal extracts the query's terms exactly as the gather would ([gin.md §6](gin.md)), including the
element-type conversion of `c`. Array `=` keeps the structural equality estimate as a logical leaf;
only its GIN candidate uses the terms (below). Then:

- **Provably empty** (a NULL operand, an unmatchable element, `?|` with no non-NULL key): zero rows.
- **Full-scan fallback** (`@> '{}'`, `?&` with no non-NULL key): no term estimate; §7 of
  [estimator.md](estimator.md) applies.
- **ALL** (`@>`, `= ANY`, `?`, `?&`, and array `=`'s candidate): order the distinct terms by
  ascending `term_rows`, then ascending bytes. With `r1` the first, fold the rest under independence:

  ```text
  rows = r1;  entries = r1
  for each later term t:  entries += rows;  rows = scale_ceil(rows, term_rows(t) / N)
  ```

  `entries` models the skip-scan ([gin.md §6.2](gin.md)): the rarest posting list is read whole,
  and each later term is probed at no more than the surviving candidates.
- **ANY** (`&&`, `?|`): `rows = min(current_nonnull, Σ term_rows)` and `entries = Σ term_rows`, both
  saturating — the disjoint-union upper bound of §7.1 of [estimator.md](estimator.md).

The predicate's logical selectivity is `rows / N`. A GIN candidate over the same column uses `rows`
as its scan rows, `entries` as its `gin_entry` estimate, and
`clamp(scale_ceil(index_nodes, entries / total_entries), index_height, index_nodes)` index pages
(zero when `entries` is zero), in place of the access-method default. An unbound parameter or a sibling-column
operand keeps the structural `matching` default; a bound parameter is a literal in its custom plan
([estimator.md §6.1](estimator.md)).

An equality join between two bare distribution-eligible columns with facts on both sides uses:

```text
ceil(left_nonnull × right_nonnull / max(left_ndv, right_ndv))
```

with saturation. MCV-to-MCV join skew is deferred. GROUP BY/DISTINCT over bare columns uses the
product of each column's `current_ndv + (current_null > 0 ? 1 : 0)`, capped by input rows; any other
expression keeps `default_distinct_values`. Multi-column dependency statistics are deferred.

## 7. Persistence — format version 29 (GIN terms: v42)

Persistent statistics are ordinary kind-tagged catalog entries after the table-entry group. A
single catalog entry must still fit one page. `entry_kind = 4` has a `stats_kind` subtag:

- `0` summary: table name, column ordinal, flags (`stale`, distribution eligibility), analyzed/null/
  width/NDV counts, sample counts, and declared MCV/histogram entry counts;
- `1` MCV: table name, column ordinal, MCV ordinal, sample frequency, and one typed non-NULL value;
- `2` histogram: table name, column ordinal, bound ordinal, and one typed non-NULL value;
- `3` term (v42): table name, column ordinal, term ordinal, row count, and the term's bytes.

Since v42 a summary's flags carry a `term_eligible` bit, and a term-eligible summary also records
`term_occurrences`, `untracked_term_rows`, and its declared term count. Term bytes are at most 128,
so a term entry fits the minimum page as an MCV entry does.

The 128-byte retained-value limit plus 63-byte identifier limit guarantees each value entry fits the
minimum 256-byte page payload. Entries sort by `(table name bytes, column ordinal, stats_kind,
ordinal)`. On load, summaries must be unique; referenced tables/ordinals must exist; counts and
ordinals must be exact/contiguous; MCV values must be distinct and in canonical frequency/key order;
histogram bounds must be nondecreasing; every value must decode under the column type and satisfy
the size/type gates; terms must be distinct, nonempty, at most 128 bytes, and in descending
row-count / ascending byte order, with each row count in `1..=analyzed_rows - null_count`; the
`term_eligible` bit must equal the column's eligibility. Any violation is `XX001`.

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
    histogram_count  i32 NOT NULL,
    term_count       i32
)
```

`term_count` is the persisted term-list length for a term-eligible column (§4.5) and NULL
otherwise.

One row exists per collected column, ordered internally by table name (byte order) then column ordinal.
It charges one `generated_row` at the source, no page/storage row units, is independently SELECT-
gated as `jed_statistics`, and scopes through `main`/`temp`/attachments like the other catalog
relations. Typed MCV/histogram arrays stay internal in the current surface.

## 9. Conformance obligations

- Shared SQL corpus: surface/errors, selective column replacement, stale retention/scaling,
  transaction rollback, temp/attachment scope, `jed_statistics`, EXPLAIN plan/estimate flips at
  equal row counts, actual costs, and NoREC equivalence.
- Per-core tests: exact collection/KMV/MCV/histogram vectors, prepared-cache fresh-vs-hit parity,
  corruption rejection, host privilege/read-only classification, and statement rollback under a
  cost ceiling.
- File format: v29 golden isolating fresh and stale stats plus cross-core/Ruby byte identity; a v42
  golden pinning array and `jsonb` term facts, including a truncated list.
- Benchmarks: uniform and skewed distributions before/after ANALYZE with identical checksums.

PostgreSQL is the SQL-result/surface oracle for the supported command form. The deterministic sample,
integer estimator, `jed_statistics`, persisted stale bit, and plan choices are jed-owned contracts.
