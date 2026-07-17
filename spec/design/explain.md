# EXPLAIN — design

> `EXPLAIN` renders jed's selected physical plan as a deterministic ordinary result set.
> `ANALYZE` executes that plan and reports deterministic meter cost, never wall-clock time. This is
> a jed-owned surface: PostgreSQL supplies behavioral precedent for planning and privilege checks,
> but jed does not reproduce PostgreSQL's plan text.

The grammar is canonical in [grammar.ebnf](../grammar/grammar.ebnf), estimator arithmetic in
[estimator.md](estimator.md), and execution units in [cost.md](cost.md). The shared corpus is the
cross-core contract; no core is the reference renderer.

## 1. Surface and options

Both spellings are accepted:

```sql
EXPLAIN [ANALYZE] <statement>
EXPLAIN (<option> [, ...]) <statement>
```

An option is `ANALYZE`, `VERBOSE`, `COSTS`, or `LANE`, optionally followed by `TRUE`, `FALSE`,
`ON`, or `OFF`. Omitting the value means true. Defaults are `ANALYZE FALSE`, `VERBOSE FALSE`,
`COSTS TRUE`, and `LANE FALSE`. A duplicate or unknown option, an empty list, or a malformed value
is `42601`. The legacy positional `EXPLAIN ANALYZE` remains equivalent to
`EXPLAIN (ANALYZE TRUE)`.

The option words are recognized positionally and remain non-reserved. The inner statement is a
query (`SELECT`, `VALUES`, a set operation, or `WITH`, including a data-modifying `WITH`) or DML
(`INSERT`, `UPDATE`, `DELETE`). DDL, transaction control, and nested `EXPLAIN` are rejected `42601`.

- Plain `EXPLAIN` plans and renders without executing. It is a read even when its inner statement
  is DML, but checks the inner statement's privileges.
- `ANALYZE` executes the inner statement. It is a write when the inner statement is a write, and a
  successful autocommit invocation commits that mutation.
- Bind values are not accepted by plain EXPLAIN; unresolved `$N` sources render symbolically.

## 2. Output shape

The first three columns are always present:

| column | type | meaning |
|---|---|---|
| `depth` | `i32` | 0-based nesting in a pre-order DFS |
| `node` | `text` | fixed operator label (§4) |
| `detail` | `text` | canonical attributes, or `-` |

Options append columns in this fixed order:

| condition | appended column(s) | type | meaning |
|---|---|---|---|
| `COSTS TRUE` (default) | `est_rows`, `est_cost` | `i64`, `i64` | rows delivered to the parent and cumulative scheduled estimate |
| `ANALYZE TRUE` | `actual_cost` | `i64` | measured inclusive cost attributable through this node |
| `LANE TRUE` | `lane` | `text` | statement-wide `streaming`, `buffered`, or `deferred` execution lane |

Thus bare EXPLAIN preserves its five-column compatibility shape. `COSTS FALSE` leaves the three
structural columns; ANALYZE and LANE are independent of COSTS.

Every cell is non-empty and has no leading or trailing whitespace. Indentation is represented by
`depth`, not spaces. Rows are a deterministic pre-order traversal and are therefore asserted with
`nosort` even though no SQL `ORDER BY` is present.

Estimate cells are saturated non-negative `i64` values. They are heuristics, not safety limits and
not promises to equal execution. Complete arithmetic and semantic CTE attribution are specified in
[estimator.md §8.3/§11](estimator.md).

## 3. ANALYZE and actual attribution

ANALYZE prepends an `Analyze` row whose detail is `cost=<C> rows=<R>` and shifts the rendered plan
one level deeper. `C` is the inner statement's exact meter total. `R` is returned rows, or affected
rows for DML without `RETURNING`. The wrapper repeats the planned root estimate when COSTS is on and
has `actual_cost = C`.

Every plan row also has an inclusive `actual_cost`. The execution profiler records leaf scan
sub-meter deltas and inclusive checkpoints after joins, residual filters, aggregation/HAVING,
windows, sort-key evaluation, DISTINCT, emission/projection, and DML target filtering. Parent
checkpoints include their executed children; a SELECT root is sampled after output projection and
`row_produced` charges. Metadata-shaped trees use execution semantics: an unexecuted definition
contributes no work, materialized work is charged once, and an inlined definition wrapper contributes
no separate work; its definition subtree and reference checkpoint expose the execution work. No
wall-clock, allocation, or host iteration value enters attribution. Query-plan frames are structural
identities, so equal labels in an outer query and derived/CTE/set-op child cannot exchange costs.
An uncorrelated expression subquery has no visible node after folding; its once-only work enters the
inclusive checkpoint of the containing visible operator (for example `Filter`) and every parent.

The EXPLAIN statement has a separate outcome cost: one `row_produced` per displayed plan row. The
inner cost appears only in `actual_cost` and the Analyze detail; it is not added to EXPLAIN's own
outcome cost.

## 4. Nodes, order, and lanes

The fixed node vocabulary is:

| source | node |
|---|---|
| base/catalog scan | `Scan <table>` / `Catalog Scan <name>` |
| SRF, CTE reference, derived relation | `SRF <name>` / `CTE Scan <name>` / `Subquery <alias>` |
| joins | `Nested Loop` / `Hash Join` |
| SELECT pipeline | `Filter`, `Aggregate`, `Window`, `Distinct`, `Sort`, `Limit`, `Result` |
| relation/set wrappers | `Values`, `Union`, `Intersect`, `Except`, `WITH`, `CTE <name>` |
| DML | `Insert <table>`, `Update <table>`, `Delete <table>` |
| ANALYZE wrapper | `Analyze` |

A SELECT renders outermost first in this order: Limit → non-elided Sort → Distinct → Window →
Aggregate → Filter → FROM tree. The FROM tree is the selected left-deep physical join order. A sort
served by scan/join order is omitted and its `ordered:` note is placed on the FROM root.

`LANE TRUE` repeats one statement-level tag on every row:

- `streaming`: the selected simple bounded pull-scan path can deliver rows incrementally;
- `deferred`: a read-only top-level WITH or set-operation boundary performs its work on first pull; and
- `buffered`: every other shape, including DML and blocking query pipelines.

The tag describes the public lazy-query dispatch lane, not an estimate and not whether an
individual child happens to use a cursor internally. Public statement write classification takes
precedence: a SELECT containing `nextval`/`setval`, or a top-level WITH containing DML, is `buffered`.

## 5. Detail and expression spelling

Attributes are separated by `; `. A node with no attributes is `-`.

- Scans render `Full scan`, `PK bound: ...`, `Index bound: using <index>`, `GIN bound: ...`,
  `GiST bound: ...`, or an interval-set form. Bound operators are `= <> < > <= >=`; sources are
  `$N`, `outer`, or canonical literals. Float bounds use the core's native shortest round-trip
  formatter rather than the old `<float>` placeholder.
- `touched=K` is the exact number of statically referenced stored columns. It is emitted for SELECT
  scans and UPDATE/DELETE target scans, including assignment sources and the storage-reading side of
  `RETURNING`; it is omitted when zero.
- Sort details are `keys=N` and optionally `top-k=K`. Limit details are `limit=N` / `offset=N`.
  Aggregate, Window, Values, set-op, CTE, and DML counts retain their established compact grammar.
- Without VERBOSE, residual filters, HAVING, and join predicates retain compact `conjuncts=N`
  counts for compatibility.
- With VERBOSE, those counts become `filter=<expr>`, `having=<expr>`, or `on=<expr>`; the outermost
  SELECT node also appends `output=[<expr>, ...]`.

The verbose printer consumes resolved expressions, not source text. It uses zero-based column slots
(`#0`), `outer(level,index)`, one-based parameters (`$1`), lowercase function/operator names,
canonical quoted constants, and full parentheses around compound operators. It prints every
resolved expression variant explicitly; adding a new resolved variant without a printer arm is a
core implementation error, not a generic fallback string. This makes aliases, whitespace, and
parser spelling irrelevant while keeping structure byte-assertable. SQL/JSON query functions spell
their resolved `RETURNING` type, wrapper and quotes mode where applicable, and `ON EMPTY`/`ON ERROR`
behaviors, including semantic defaults.

## 6. Data-modifying WITH

Plain EXPLAIN plans a top-level data-modifying WITH without running any sub-statement. The `WITH`
root contains each `CTE <name>` in authored order. A data-modifying CTE is always `materialized` and
its complete Insert/Update/Delete subtree appears below its definition; the primary query or DML
subtree follows the definitions. Reference counts still determine read-only CTE modes. ANALYZE uses
the ordinary writable-CTE orchestrator, including its pre-statement read pin and last-write-wins
rules.

## 7. Determinism ledger

Plan traversal, names, counts, integer literals, and non-float constant rendering remain exact
cross-core contracts. Two newly exposed presentation surfaces are separately audited in
[determinism_exceptions.toml](../conformance/determinism_exceptions.toml):

- `explain-expression-spelling` records the hand-written resolved-expression spelling contract. It
  drops no guarantee; exact corpus cells are the differential check.
- `explain-float-literal-layout` bounds the already-ratified native shortest-float layout exception
  to float text embedded in EXPLAIN details. The float value and selected bound remain identical;
  only equivalent exponent/layout spelling may vary at formatter-specific thresholds.

No hashmap iteration, locale, clock, entropy, allocation, or wall time may reach plan text,
estimates, actual costs, or lane tags.

## 8. PostgreSQL divergences and remaining follow-ons

- The structured result format, node vocabulary, deterministic cost units, actual-cost column, and
  LANE option are jed-owned rather than PostgreSQL plan-text compatibility surfaces.
- jed supports only boolean ANALYZE/VERBOSE/COSTS/LANE options. PostgreSQL FORMAT, BUFFERS, TIMING,
  SUMMARY, WAL, SETTINGS, GENERIC_PLAN, MEMORY, and SERIALIZE are not accepted.
- ANALYZE reports deterministic logical work instead of timing.

Collation-name rendering in keys and additional physical operators remain follow-ons; none changes
the contracts above.
