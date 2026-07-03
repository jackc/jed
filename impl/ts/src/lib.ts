// Public entry point of the TypeScript core (CLAUDE.md §2): a downstream consumer of
// /spec, the canonical source of truth. Runs natively on modern Node via type-stripping
// — no build step. i64 is exact (uniform bigint); the on-disk format is byte-identical
// to the Rust and Go cores (CLAUDE.md §8).

// SUPPORTED_CAPABILITIES lists the capabilities this core implements (spec/conformance:
// the gating axis). The harness runs a corpus file iff every capability in the file's
// `# requires:` header is in this set. Identical to the Rust/Go cores — full parity.
export const SUPPORTED_CAPABILITIES: readonly string[] = [
  // CREATE TABLE with typed columns + single-column PRIMARY KEY.
  "ddl.create_table",
  "ddl.primary_key",
  // Table-level PRIMARY KEY (a, b, ...) — composite keys (constraints.md §3).
  "ddl.composite_primary_key",
  "ddl.check",
  // FOREIGN KEY constraints — referential integrity enforced at every write, 23503; referenced
  // columns must be a parent PK/UNIQUE set (42830); persisted (format_version 11)
  // (constraints.md §6, grammar.md §43).
  "ddl.foreign_key",
  // DROP TABLE — remove a table (definition + rows) from the catalog (grammar.md §13).
  "ddl.drop_table",
  // DROP TABLE IF EXISTS — the idempotent form: a missing table is a no-op success rather
  // than 42P01 (a non-table relation is still the 42809 wrong-object-type) (grammar.md §13).
  "ddl.drop_table_if_exists",
  // DROP TABLE a, b [CASCADE | RESTRICT] — multi-table (two-phase / all-or-nothing, repeated name
  // deduplicated) + the FK-dependency mode: RESTRICT (default) refuses an external FK dependent
  // (2BP01), CASCADE drops it; a FK within the drop set never blocks (grammar.md §13).
  "ddl.drop_table_multi",
  // CREATE [TEMP|TEMPORARY] TABLE + DROP — session-local temporary tables that make zero writes to the
  // database file (held in a per-session temp snapshot outside the serialized catalog, no
  // format_version change). Full CRUD + PK/UNIQUE/CHECK/NOT NULL/DEFAULT, dropped at session close,
  // preclude-overlaps (42P07), standalone CREATE INDEX / DROP INDEX (the index in the temp snapshot,
  // gated by the same allow_temp_ddl), serial/IDENTITY columns (the OWNED sequence lives in the temp
  // snapshot — zero file writes — and is auto-dropped with the table), and composite-typed columns
  // (resolved against the persistent type catalog; DROP TYPE of a referenced type is 2BP01). Collated
  // columns and FK on a temp table are deferred 0A000 (spec/design/temp-tables.md §8). Gate
  // session.allow_temp_ddl; storage budget resource.temp_budget.
  "ddl.temp_table",
  // CREATE SHARED [TEMP|TEMPORARY] TABLE — database-wide shared temporary tables (visible to every
  // session of the open Engine, sharing one set of rows), still making zero file writes (held in
  // the Engine-level shared-temp snapshot; the two-root commit, temp-tables.md §4/§5). Same feature
  // set + 0A000 narrowings as ddl.temp_table; cross-session visibility tested via the concurrency
  // schedule format. Gate session.allow_shared_temp_ddl; budget resource.shared_temp_budget.
  "ddl.shared_temp_table",
  // CREATE INDEX / DROP INDEX — non-unique secondary indexes, maintained on every write
  // and used to bound SELECT scans (spec/design/indexes.md, grammar.md §30).
  "ddl.secondary_index",
  "ddl.unique",
  // GIN inverted indexes — CREATE INDEX ... USING gin over an integer-element array column;
  // built + maintained on every write, persisted (format_version 13). The query-side planner
  // bound is query.gin_scan. spec/design/gin.md
  "ddl.gin_index",
  // GiST indexes — CREATE INDEX ... USING gist over a range column (range_ops); the on-disk R-tree
  // (pages 5/6, format_version 20). Query-side bound is query.gist_scan. spec/design/gist.md GX1
  "ddl.gist_index",
  // GiST scalar `=` opclass (the in-core btree_gist, spec/design/gist.md GX2): CREATE INDEX …
  // USING gist over a fixed-width keyable scalar column; bounds are [min,max] over the
  // order-preserving key encoding. The query-side planner bound is query.gist_scalar_scan.
  "ddl.gist_scalar_index",
  // EXCLUDE constraints (spec/design/gist.md §7, GX3): EXCLUDE [USING gist] (col WITH op, …),
  // backed by a multi-column GiST index; the conjunction probe traps 23P01 at INSERT/UPDATE, with
  // the NULL rule + end-state semantics. format_version 21.
  "ddl.exclusion_constraint",
  // Composite (row) types — CREATE TYPE / DROP TYPE, persisted (format_version 9); composite
  // columns/values are a later slice (spec/design/composite.md).
  "types.composite",
  "types.array",
  // Array element subscript a[i] — 1-based, OOB/NULL → NULL, non-array base 42804 (array.md §6).
  "expr.array_subscript",
  // Multidimensional array values + custom lower bounds (array.md §12).
  "types.array_multidim",
  // Array slices a[m:n] (array.md §6) — sub-array reads, renumbered to lower bound 1.
  "expr.array_slice",
  // Array-of-composite element types (array.md §12 AC1) — a composite is a first-class array
  // element type (addr[]); the per-element compare routes through the composite total order.
  "types.array_composite",
  // The three array-involving casts (array.md §7): array → text (array_out), runtime text → T[]
  // (array_in per row), and element-wise array → other-element-array (each element through the
  // scalar cast, for the casts.toml-admitted element pairs). array → text is explicit-only.
  "cast.array",
  // A composite type with an array-typed field (array.md §12 — the mirror nesting) — the catalog
  // composite-type entry gains a code-15 array field; the codec/comparison/text-I/O recurse.
  "types.composite_array_field",
  // Range types (ranges.md) — the six built-in PG range types as a structural type over a scalar
  // element (R0–R2): the '[1,5)' literal/cast, the value codec (type_code 17, format_version 15),
  // range_out, discrete canonicalization, empty normalization, IS NULL. Comparison + the
  // constructor/operator surface land in R3 / RF1–RF4.
  "types.range",
  // json/jsonb literal-only surface (J0, spec/design/json.md §12): json_in/out + jsonb_in/out +
  // the '…'::json / '…'::jsonb literal cast + jsonb_out canonicalization. No storable column yet
  // (a json/jsonb column is 0A000 until J1).
  "types.jsonb_literal",
  // Storable jsonb column (J1) — canonical tagged-node value body (type_code 19), format_version
  // 19, golden jsonb_table.jed; a bare string literal adapts; spills via the large-value path.
  "types.jsonb",
  // Storable json column (J1b) — verbatim text value body (type_code 18), golden json_table.jed.
  "types.json",
  // jsonb comparison/ordering (J2) — PG total btree order driving =/<>/</<=/>/>=/ORDER BY/
  // DISTINCT/GROUP BY; json non-comparable → 42883 (spec/design/json.md §5).
  "types.jsonb_compare",
  // The JSON cast matrix (J3) — runtime json↔jsonb, json/jsonb→text, text→json/jsonb
  // (spec/design/json.md §6.1).
  "types.json_casts",
  // The jsonpath scalar type (P1a, spec/design/jsonpath.md): a first-class type (reserved type
  // code 20) built from a '…'::jsonpath / jsonpath '…' literal, compiled at resolve and rendered
  // to its canonical normalized form. LITERAL-ONLY (a jsonpath column is 0A000; a jsonpath value is
  // NOT comparable → 42883). A valid-PG filter / item method / arithmetic / $name is a deferred
  // 0A000 (P1b); a malformed path literal is 42601.
  "types.jsonpath",
  // The jsonpath query functions (P2) — jsonb_path_exists / jsonb_path_query / jsonb_path_query_first
  // / jsonb_path_query_array (jsonpath.md §5).
  "func.jsonb_path",
  // jsonpath filter expressions `?(predicate)` + the `@?` exists operator (P1b, jsonpath.md §4/§6):
  // a filter step keeps items whose predicate is definitely TRUE (3-valued Kleene connectives,
  // existential comparison); `jsonb @? jsonpath` is jsonb_path_exists. Item methods / like_regex /
  // arithmetic stay deferred 0A000.
  "expr.jsonpath_filter",
  // jsonpath top-level predicates + jsonb_path_match + the `@@` operator (P1b/P2, jsonpath.md §6):
  // a jsonpath body can be a boolean predicate (`$.a == 1`) producing a single boolean item;
  // `jsonb_path_match` / `jsonb @@ jsonpath` returns it (22038 if not exactly one boolean item).
  "expr.jsonpath_match",
  // jsonb accessor operators (J4) — `->` `->>` `#>` `#>>` (json-sql-functions.md §1).
  "func.jsonb_access",
  // jsonb containment / key-existence (J5) — `@>` `<@` `?` `?|` `?&` (json-sql-functions.md §1).
  "func.jsonb_contains",
  // jsonb mutation operators (J6) — `||` `-` `#-` (json-sql-functions.md §1).
  "func.jsonb_mutate",
  // json/jsonb introspection + processing functions (B1) — json[b]_typeof / _array_length /
  // _strip_nulls / jsonb_pretty (json-sql-functions.md §2).
  "func.json_processing",
  // json/jsonb single-column SRFs (B2) — jsonb_array_elements[_text] / json[b]_object_keys
  // (json-sql-functions.md §3).
  "func.json_srf",
  // json/jsonb two-column SRFs (B3) — jsonb_each / jsonb_each_text (json-sql-functions.md §3); the
  // C0 multi-column synthetic table. The json variants are a deferred 0A000 follow-on.
  "func.json_each",
  // The FROM-clause column-definition list `AS t(col type, …)` (C0, json-table.md §1) — the typed
  // output columns a record-returning function declares; the shared parseFieldDefList body.
  "func.coldeflist",
  // json[b]_to_record / json[b]_to_recordset (R1, json-table.md §2) — the record-returning functions
  // that map a JSON object's members to the C0 col-def-list columns by name + coerce.
  "func.json_record",
  // json[b]_populate_record / json[b]_populate_recordset (R2, json-table.md §2) — like the R1 record
  // functions but the column shape comes from the COMPOSITE TYPE of the (typically NULL) first argument.
  "func.json_populate",
  // to_jsonb(anyelement) — the JSON image of a value (B1 builder subset, json-sql-functions.md §2).
  "func.to_jsonb",
  // to_json + json[b]_build_array / json[b]_build_object (B1, json-sql-functions.md §2) — the
  // json/jsonb construction builders over the same valueToNode element kernel.
  "func.json_builders",
  // jsonb_set / jsonb_insert (json-sql-functions.md §2) — the jsonb path-mutation builders, resolved
  // by name (not in the catalog): a text[] path of object keys / array indices, STRICT.
  "func.json_set",
  // json_object / jsonb_object (json-sql-functions.md §2) — object builders from text array(s),
  // resolved by name (not in the catalog): one text[] of alternating keys/values, or two text[].
  "func.json_object",
  // json[b]_agg / json[b]_agg_strict (B4) — the JSON-array aggregate builders (json-sql-functions.md
  // §4). Aggregate a group's values into one JSON array via the to_jsonb element kernel.
  "func.json_agg",
  // json[b]_object_agg[_unique] (B4) — the JSON-object aggregate builders (json-sql-functions.md §4).
  // Aggregate a group's (key, value) pairs into one JSON object; _unique errors 22030 on a dup key.
  "func.json_object_agg",
  // The IS JSON predicate (S1) — expr IS [NOT] JSON [VALUE|SCALAR|ARRAY|OBJECT] [(WITH|WITHOUT)
  // UNIQUE [KEYS]], the SQL/JSON well-formedness test (json-sql-functions.md §5).
  "expr.is_json",
  // The SQL/JSON constructors (S1) — JSON(text [(WITH|WITHOUT) UNIQUE [KEYS]]), JSON_SCALAR(v),
  // JSON_SERIALIZE(json|jsonb) (json-sql-functions.md §5).
  "func.json_ctor",
  // The SQL/JSON query functions (S2) — JSON_EXISTS / JSON_VALUE / JSON_QUERY, the keyword-led
  // primaries that compile a jsonpath, evaluate it over a context item, and apply per-function
  // semantics (existence / a single scalar / a json value) with RETURNING/wrapper/quotes/ON
  // EMPTY/ON ERROR sub-clauses (json-sql-functions.md §5).
  "expr.json_query_fns",
  // JSON_TABLE with the default plan (T1, json-table.md §3): COLUMNS, FOR ORDINALITY, EXISTS, NESTED.
  "func.json_table",
  // Range accessor functions RF1 (range-functions.md §1): the polymorphic anyrange resolution +
  // the seven STRICT readers lower/upper/isempty/lower_inc/upper_inc/lower_inf/upper_inf.
  "func.range_accessors",
  // Range constructor functions RF2 (range-functions.md §2): the six range constructors
  // i32range/i64range/numrange/tsrange/tstzrange/daterange (plus int4range/int8range aliases) in the
  // 2-arg (lo, hi) and 3-arg (lo, hi, bounds) forms; a NULL bound is an infinite bound (non-strict).
  "func.range_constructors",
  // Range boolean operators RF3 (range-functions.md §3): @> <@ && (shared with arrays, + the range
  // @> element / element <@ range overloads) and the range-only << >> &< &> -|-, each
  // anyrange<op>anyrange → boolean; STRICT, definite boolean, same-element-type operands.
  "func.range_operators",
  // Range set operators RF4 (range-functions.md §4): + union, * intersection, - difference (the
  // arithmetic tokens, dispatched by range operand) and range_merge, each anyrange<op>anyrange →
  // anyrange; STRICT; + and - raise 22000 on a non-contiguous result, * and range_merge never error.
  "func.range_set_operators",
  // Array function/operator surface AF1 (array-functions.md): the polymorphic anyarray/anyelement
  // resolution + introspection (array_ndims/length/lower/upper/cardinality/dims) + builders
  // (array_append/prepend/cat).
  "func.array",
  // Regex scalar functions (regex.md §8): regexp_replace → text and regexp_match → text[], over the
  // same Pike VM as the operators; the first text- and text[]-returning scalar functions.
  "func.regexp_replace",
  "func.regexp_match",
  // Oracle-compat regex scalar functions (regex.md §8b): regexp_like → boolean, regexp_count →
  // integer, regexp_substr → text, regexp_instr → integer, over the same Pike VM.
  "func.regexp_like",
  "func.regexp_count",
  "func.regexp_substr",
  "func.regexp_instr",
  // Array function surface AF3 (array-functions.md §9): the polymorphic SRF unnest(anyarray) — a
  // FROM-clause row source expanding an array into one row per element (functions.md §10).
  "func.unnest",
  // Array function surface AF4 (array-functions.md §10): the containment/overlap operators
  // @>/<@/&& — polymorphic `anyarray <op> anyarray → boolean`, strict element equality.
  "func.array_containment",
  // Array function surface AF5 (array-functions.md §11): the ANY/ALL/SOME quantified array
  // comparisons (x = ANY(arr), x op ALL(arr)) — the array spelling of IN, three-valued.
  "func.array_quantified",
  // Array function surface AF6 (array-functions.md §12): the VARIADIC call syntax + variadic
  // overload resolution — the num_nulls/num_nonnulls built-ins (spread or VARIADIC-array form).
  "func.variadic",
  // Array function surface AF7 (array-functions.md §13): the whole AF1–AF6 surface over a
  // COMPOSITE element type + unnest(composite[]) — the quantifiers use the composite total order.
  "func.array_composite",
  // Sequences — CREATE SEQUENCE / DROP SEQUENCE, a persisted i64 generator (format_version 12),
  // and the value functions nextval/currval (transactional advance — sequences.md §5).
  "ddl.sequence",
  "func.sequence",
  // serial / bigserial / smallserial CREATE TABLE pseudo-types — an owned sequence + DEFAULT
  // nextval(...) + NOT NULL; DROP TABLE auto-drops it; format_version 14 (sequences.md §12).
  "ddl.serial",
  // GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY columns + the INSERT OVERRIDING clause —
  // serial's machinery + ALWAYS/BY DEFAULT gating; format_version 15 (sequences.md §13).
  "ddl.identity",
  // CREATE SEQUENCE … AS { smallint | integer | bigint } — the sequence value type sets the
  // default + validated MIN/MAX bounds; serial/identity follow the column type (sequences.md §14).
  "ddl.sequence_as_type",
  // ALTER SEQUENCE … <options> / RENAME TO — re-edit the definition (PG init_params, isInit=false)
  // or move the catalog key (rewriting an owned sequence's nextval default); no format change
  // (sequences.md §15).
  "ddl.alter_sequence",
  // NOT NULL column constraint — storing NULL traps 23502 (spec/design/constraints.md §1).
  "ddl.not_null",
  // DEFAULT <literal> column constraint, evaluated + coerced at CREATE (constraints.md §2).
  "ddl.column_default",
  // DEFAULT <expression> column constraint — a non-constant default (e.g. uuidv7(), 1 + 1)
  // evaluated per row at INSERT (spec/design/constraints.md §2).
  "ddl.column_default_expr",
  // INSERT with an explicit column list + the DEFAULT keyword (grammar.md §12).
  "dml.insert_column_list",
  // INSERT ... VALUES with positional type-checking + overflow trap.
  "dml.insert",
  // Multi-row INSERT ... VALUES (..),(..) — two-phase / all-or-nothing (grammar.md §12).
  "dml.insert_multi_row",
  // INSERT ... SELECT — insert the rows a query produces; up-front arity (42601) +
  // type-assignability (42804) gates, then the same two-phase validation (grammar.md §24).
  "dml.insert_select",
  "error.overflow_trap",
  // Row mutation: UPDATE (in-place) + DELETE.
  "dml.update",
  // Assigning a range/array (container) column in an UPDATE (ranges.md §4 / array.md §4).
  "dml.update_container",
  "dml.delete",
  // The RETURNING clause on INSERT/UPDATE/DELETE — the statement becomes a query result
  // projecting each affected row (grammar.md §32, cost.md §3).
  "dml.returning",
  // The old./new. row-version qualifiers in a RETURNING list (PG 18 semantics): old.col =
  // the pre-statement value, new.col = the post-statement value, the absent side the
  // all-NULL row (grammar.md §32).
  "dml.returning_old_new",
  // INSERT ... ON CONFLICT [target] { DO NOTHING | DO UPDATE SET … [WHERE …] } — UPSERT
  // (spec/design/upsert.md, grammar.md §46): arbiter inference / ON CONSTRAINT, the
  // `excluded` pseudo-relation, the 21000 second-affect rule, non-arbiter 23505.
  "dml.insert_on_conflict",
  // SELECT, WHERE (=, ordering), ORDER BY, IS [NOT] NULL, 3VL, casts, cross-type
  // comparison via the promotion tower, and all three integer types.
  "query.select",
  "query.where_eq",
  "query.comparison_order",
  "query.point_lookup",
  "query.or_in_point_lookup",
  "query.limit_short_circuit",
  "query.order_by_pk_scan",
  "query.order_by_index_scan",
  "query.order_by_join_scan",
  "query.correlated_pushdown",
  "query.join_pushdown",
  "query.index_nested_loop",
  // Index range scans + multi-column prefix bounds (spec/design/indexes.md §5.1).
  "query.index_range",
  "query.index_prefix",
  // EXPLAIN — render the planner's chosen plan as a deterministic depth/node/detail result set,
  // without executing the inner statement (spec/design/explain.md).
  "query.explain",
  // EXPLAIN of a DML statement (INSERT/UPDATE/DELETE), plan-only — renders without mutating.
  "dml.explain",
  // EXPLAIN ANALYZE — run the inner statement and report its actual accrued cost + row count on
  // an Analyze root (query = read; dml = the mutation runs and commits). spec/design/explain.md §3
  "query.explain_analyze",
  "dml.explain_analyze",
  // GIN-bounded scan — `col @> const` / `col && const` over a GIN-indexed array column narrows
  // the SELECT scan to candidate rows (term gather → intersect/union → residual filter); the
  // result is identical to the full scan (spec/design/gin.md §6).
  "query.gin_scan",
  // GiST-bounded scan — `range_col && const` / `range_col @> const` over a GiST-indexed range column
  // descends the resident R-tree to candidate rows; the predicate stays the residual filter (same
  // rows as the full scan, lower cost — spec/design/gist.md §5).
  "query.gist_scan",
  // GiST scalar `=` bounded scan (spec/design/gist.md §6): a `scalar_col = const` against a
  // GiST-indexed fixed-width scalar column descends the resident R-tree to candidate rows; the
  // `=` predicate stays the residual filter. The fallback bound — a PK/B-tree column wins.
  "query.gist_scalar_scan",
  // GIN-bounded `c = ANY(col)` membership — the single-term @> reduction over a GIN-indexed
  // array column (spec/design/gin.md §6); same rows as the full scan, lower cost.
  "query.gin_any_eq",
  // GIN-bounded array equality `col = Q` — the `@> distinct(Q)` superset gather + residual = over
  // a GIN-indexed array column (spec/design/gin.md §6); same rows as the full scan.
  "query.gin_array_eq",
  // GIN-bounded UPDATE/DELETE — a mutation whose WHERE has a GIN-accelerable conjunct bounds its
  // target-row scan through the GIN index (PK-then-GIN-then-full), same end state as the full
  // scan (spec/design/gin.md §6); the ordered-index bound stays SELECT-only.
  "query.gin_mutation",
  // GIN over non-integer fixed-width key-encodable element types (uuid/date/timestamp/
  // timestamptz/boolean) — the gate lift + the shared key-value term encoder (gin.md §3/§4).
  "query.gin_element_types",
  "query.is_null",
  "query.order_by",
  // Richer ORDER BY — multiple keys, per-key ASC/DESC, per-key NULLS FIRST|LAST (grammar.md §10).
  "query.order_by_keys",
  // ORDER BY by an output-column ordinal (`ORDER BY 1`), incl. the set-op ORDER BY (grammar.md §10).
  "query.order_by_ordinal",
  // ORDER BY by a general expression (`ORDER BY a + 1`, `ORDER BY sum(b)`), materialized (grammar.md §10).
  "query.order_by_expr",
  // ORDER BY by an output-column alias / name (`... AS s ... ORDER BY s`), output-first (grammar.md §10).
  "query.order_by_alias",
  // A correlated ORDER BY key — an inner ORDER BY referencing an enclosing-query column (grammar.md §26).
  "query.order_by_correlated",
  // A window function inside an ORDER BY key (`ORDER BY row_number() OVER (...)`, window.md §5.1).
  "query.order_by_window",
  // A GROUPING() call inside an ORDER BY key (`ORDER BY GROUPING(a)`, aggregates.md §12).
  "query.order_by_grouping",
  // A general-expression ORDER BY key in a grouped query that ALSO has window functions (window.md §5.2).
  "query.order_by_grouped_window",
  // Select-list output naming: SELECT *, AS aliases, and the ?column? rule (grammar.md §8).
  "query.select_star",
  "query.column_alias",
  // LIMIT / OFFSET row windowing, applied after ORDER BY, before projection (grammar.md §9).
  "query.limit",
  "query.offset",
  // SELECT DISTINCT: deduplicate projected output rows, NULL-safe (grammar.md §11).
  "query.distinct",
  // FROM-less SELECT: one virtual zero-column row, no scan cost (grammar.md §34).
  "query.select_no_from",
  // Set-returning functions in FROM: generate_series, a computed row source (functions.md §10).
  "query.set_returning",
  // Phase 4 — multi-table FROM: INNER/CROSS/OUTER JOIN, table aliases, qualified columns
  // (grammar.md §15).
  "query.join_inner",
  "query.cross_join",
  "query.comma_from",
  "query.join_using",
  "query.join_natural",
  "query.join_left",
  "query.join_right",
  "query.join_full",
  "query.table_alias",
  "query.qualified_column",
  "query.qualified_star",
  // Scalar aggregates COUNT/SUM/MIN/MAX/AVG over the whole table (spec/design/aggregates.md).
  "query.aggregates",
  // DISTINCT inside an aggregate — COUNT(DISTINCT x), SUM/AVG/MIN/MAX(DISTINCT x): fold only the
  // distinct non-NULL argument values, value-canonically deduped (spec/design/aggregates.md §5).
  "query.aggregate_distinct",
  // FILTER (WHERE cond) on an aggregate — agg(x) FILTER (WHERE cond): fold only the input rows for
  // which cond is TRUE (spec/design/aggregates.md §11). 42809 on a non-aggregate, 0A000 on a window.
  "query.aggregate_filter",
  // FILTER (WHERE cond) on a WINDOW aggregate — agg(x) FILTER (WHERE cond) OVER (...): fold only the
  // frame rows for which cond is TRUE (spec/design/aggregates.md §20). 0A000 on a non-aggregate window fn.
  "query.window_aggregate_filter",
  // SELECT DISTINCT in an aggregate query — dedup the projected grouped output rows by equality,
  // keeping first occurrence, then LIMIT/OFFSET (spec/design/aggregates.md §10).
  "query.aggregate_select_distinct",
  // GROUP BY: one row per grouping-key combination + the grouping-error rule + ORDER BY over
  // grouping keys (spec/design/aggregates.md §5-6, grammar.md §18).
  "query.group_by",
  // GROUP BY by a select-list ordinal (GROUP BY 1), an output alias (GROUP BY s), or a general
  // expression (GROUP BY a + b) — not just a bare column (spec/design/aggregates.md §15).
  "query.group_by_expr",
  // Functional-dependency grouping: GROUP BY a base table's full primary key lets any column of
  // that table appear ungrouped, since the PK determines it (spec/design/aggregates.md §16).
  "query.group_by_functional_dependency",
  // GROUPING SETS / ROLLUP / CUBE and the GROUPING() function — multiple grouping sets in one
  // GROUP BY (spec/design/aggregates.md §12, grammar.md §18).
  "query.grouping_sets",
  // GROUPING SETS / ROLLUP / CUBE / GROUPING() combined with window functions — the window stage
  // runs over the unioned grouping-set rows (spec/design/aggregates.md §21).
  "query.grouping_sets_window",
  // HAVING: a boolean filter over grouped rows, after aggregation, before ORDER BY
  // (spec/design/aggregates.md §8, grammar.md §19).
  "query.having",
  // Ordered-set aggregates mode() / percentile_cont(f) / percentile_disc(f) WITHIN GROUP
  // (ORDER BY col) (spec/design/aggregates.md §13, grammar.md §17).
  "query.ordered_set_aggregate",
  // A general-expression WITHIN GROUP order key (ORDER BY a + b), not just a bare column.
  "query.within_group_expr",
  // A non-constant ordered-set aggregate fraction — percentile_cont(expr) / percentile_disc(expr)
  // where expr references grouping columns, evaluated per group (spec/design/aggregates.md §17).
  "query.ordered_set_nonconstant_fraction",
  // percentile_cont over an interval WITHIN GROUP input — interpolated in the interval domain
  // (PG interval_lerp), result interval (spec/design/aggregates.md §13).
  "query.ordered_set_interval",
  // An array-valued percentile_*(ARRAY[...]) fraction — one percentile per element, result an
  // array (spec/design/aggregates.md §18).
  "query.ordered_set_array_fraction",
  // A collated WITHIN GROUP key — mode/percentile_disc honor an explicit COLLATE (or a column's
  // frozen collation) in the WITHIN GROUP sort (spec/design/aggregates.md §13).
  "query.ordered_set_collation",
  // Hypothetical-set aggregates rank/dense_rank/percent_rank/cume_dist WITHIN GROUP — the rank the
  // hypothetical direct-arg row would have in the group (spec/design/aggregates.md §19).
  "query.hypothetical_set_aggregate",
  // Window functions — OVER (PARTITION BY / ORDER BY), the window stage, row_number() (S0,
  // spec/design/window.md).
  "query.window",
  "query.window_ranking",
  "query.window_ratio",
  "query.window_ntile",
  "query.window_offset",
  "query.window_aggregate",
  "query.window_frame",
  "query.window_frame_range",
  "query.window_frame_exclude",
  "query.window_named",
  // Window functions combined with GROUP BY / aggregates in one query (window.md §2/§5.1).
  "query.window_grouped",
  // Base-window-extending definitions: OVER (w ORDER BY …) / WINDOW w2 AS (w …) (S9, window.md §5).
  "query.window_base_extend",
  "query.window_collation",
  // General-expression window PARTITION BY / ORDER BY keys (window.md §5.1).
  "query.window_expr_keys",
  // Set operations UNION / INTERSECT / EXCEPT (each [ALL]) — spec/design/grammar.md §25.
  "query.union",
  "query.intersect",
  "query.except",
  // Subqueries: scalar / IN / EXISTS, both uncorrelated (folded once) and correlated
  // (re-executed per outer row, any depth) — spec/design/grammar.md §26.
  "query.subquery_scalar",
  "query.subquery_in",
  "query.subquery_exists",
  "query.subquery_correlated",
  // Quantified comparison over a subquery — x op ANY/SOME/ALL (SELECT …), the subquery spelling
  // of IN (array-functions.md §11.6).
  "query.subquery_quantified",
  // Non-recursive common table expressions — WITH name [(cols)] AS [NOT] MATERIALIZED (query)
  // [, ...] <query> (spec/design/cte.md).
  "query.cte",
  // Nested WITH — a WITH clause prefixing any parenthesized query expression (a subquery, derived
  // table, scalar/IN/EXISTS/ANY-ALL subquery, set-op operand, or CTE body), establishing its own
  // CTE scope (spec/design/cte.md §7).
  "query.cte_nested",
  // Recursive common table expressions — WITH RECURSIVE name [(cols)] AS (anchor UNION [ALL]
  // recursive_term) <query>: the iterate-to-fixpoint (working-table) executor; cost-ceiling
  // termination (spec/design/recursive-cte.md).
  "query.cte_recursive",
  // Data-modifying (writable) CTEs — a WITH item's body may be an INSERT/UPDATE/DELETE with its
  // own optional RETURNING, feeding its RETURNING rows forward; every sub-statement reads one
  // pre-statement snapshot (a read pin), the parts run in lexical order all-or-nothing
  // (spec/design/writable-cte.md).
  "query.cte_data_modifying",
  // WITH clause on a data-modifying primary — the WITH-prefixed statement may itself be an
  // INSERT/UPDATE/DELETE (spec/design/writable-cte.md).
  "dml.with_clause",
  // Derived tables — FROM ( query_expr ) AS t: a parenthesized subquery as a FROM relation, the
  // parser surface over the CTE inline seam (an anonymous always-inlined single-ref CTE) —
  // spec/design/grammar.md §42.
  "query.derived_table",
  // VALUES-body derived table — FROM (VALUES (e…),(e…)) [AS] v(c…): a parenthesized VALUES list as
  // a FROM relation, a computed relation of literal rows (general constant expressions, per-column
  // type unification across rows) — spec/design/grammar.md §42.
  "query.values",
  // LATERAL joins — a FROM item (LATERAL derived table / VALUES, or an implicitly-lateral table
  // function) whose body / args reference the EARLIER FROM relations, a dependent join re-evaluated
  // per left-hand row — spec/design/grammar.md §44.
  "query.lateral",
  // Scalar functions abs / round (per-row, valid anywhere an expression is) —
  // spec/design/functions.md §9.
  "func.abs",
  "func.round",
  "func.casing",
  // Named-argument notation + DEFAULT parameter values, via make_interval — functions.md §11.
  "func.named_arguments",
  // Timestamp constructors make_timestamp/make_timestamptz (named, un-defaulted) — functions.md §11.
  "func.make_timestamp",
  // Pure uuid extractors (uuid_extract_version/_timestamp) — functions.md §12.
  "func.uuid_extract",
  // Volatile uuid generators (uuidv4/uuidv7) on the entropy+clock seam — entropy.md.
  "func.uuid_generate",
  // Current-time functions on the clock seam — now()/current_timestamp (STABLE) and
  // clock_timestamp() (VOLATILE) — entropy.md §5, functions.md §12.
  "func.now",
  "func.clock_timestamp",
  "null.three_valued",
  "compare.promotion",
  "cast.explicit",
  // Typed string literals — `type 'string'` and CAST(<string literal> AS type) coerce the
  // literal to the named type at resolve (spec/design/grammar.md §36, types.md §5).
  "cast.string_literal",
  // The postfix `::` cast operator — `expr :: type` desugars to CAST(expr AS type), sharing
  // its whole machinery; binds tighter than unary minus (spec/design/grammar.md §37).
  "cast.operator",
  // boolean ⇄ i32 casts (the boolean cast slice — spec/types/casts.toml, types.md §9): both
  // directions explicit, i32 only (bool↔i16/i64 is a forbidden 42804).
  "cast.bool_int",
  // uuid ⇄ other casts (the uuid cast slice — spec/types/casts.toml, types.md §14): four explicit
  // pairs, text ⇄ uuid (uuid_in / canonical lowercase) and bytea ⇄ uuid (the 16 raw bytes, a jed
  // cast PG lacks — per-core tested). text ⇄ uuid is oracle-checked.
  "cast.uuid",
  // Runtime text → numeric/boolean casts (the runtime-text-cast slice — spec/design/grammar.md
  // §36, types.md §5, casts.toml): CAST(text_expr AS T) / text_expr :: T on a NON-LITERAL text
  // expression for i16/i32/i64, decimal (typmod re-scale), f32/f64, boolean — the same per-type
  // coercion the LITERAL form folds at resolve, but per row in evalCast. Malformed → 22P02,
  // out of range → 22003. jed's own grammar (hex/underscore/NaN trap 22P02 — per-core tested);
  // the accepted-grammar cases agree with PG and are oracle-checked. text → uuid is cast.uuid;
  // text → date/timestamp/interval/bytea stay deferred.
  "cast.runtime_text",
  // The COLLATE expression operator + ORDER BY … COLLATE over a VENDORED collation (collation slice
  // 1c, spec/design/collation.md §14): a vendored collation orders text by its UCA sort key in the
  // ordering comparisons (< <= > >=) and ORDER BY; explicit-conflict 42P21, unknown 42704, non-text
  // COLLATE 42804; the `collate` cost unit.
  "expr.collate",
  // The AT TIME ZONE operator (and the timezone(zone, value) function it desugars to) + host-loaded
  // IANA time-zone data (spec/design/timezones.md §6, grammar.md §49): convert timestamptz↔timestamp
  // through a named zone or fixed offset. A zone is provided by a host-loaded JTZ bundle (the
  // `# load-timezone:` directive loads spec/tz/fixtures/tzdata.jtz); UTC and fixed offsets are built
  // in. Unknown zone 22023, non-text zone 42883; the `timezone` cost unit.
  "expr.at_time_zone",
  // The tz conversion surface (spec/design/timezones.md §9): date_trunc / EXTRACT / the cross-family
  // datetime casts, all consuming the session time_zone slot (the zone a timestamptz is decomposed
  // in). date_part / julian / text-casts / make_timestamptz deferred; rendering stays UTC (§9.5).
  "expr.date_trunc",
  "expr.extract",
  "cast.datetime",
  "session.timezone",
  // Per-column COLLATE in CREATE TABLE (collation slice 1d, spec/design/collation.md §1/§5): a
  // column's effective collation is frozen at create (text-only 42804, vendored-or-C name 42704) and
  // is its IMPLICIT collation — ORDER BY / comparisons use it with no explicit COLLATE; two different
  // implicit collations conflict 42P22. Persisted via format_version 18 (goldens, not corpus).
  "ddl.collate_column",
  "types.i16",
  "types.i32",
  "types.i64",
  // text scalar type (variable-width UTF-8, collation C): storage, literals, and
  // comparison/ordering. text is ALSO a key type — a text PRIMARY KEY / index / UNIQUE uses the
  // variable-width text-terminated-escape key encoding (encoding.md §2.4).
  "types.text",
  // varchar(n) / string(n) length limits (the second parameterized type — spec/design/types.md
  // §15): a text column/field/cast-target carries a max-length typmod (code points). Over-length
  // assignment traps 22001 (trailing-space exception truncates); an explicit ::varchar(n) cast
  // truncates. format_version 22 (text column/field u32 varchar_max_len typmod slot).
  "types.varchar_length",
  // Storable boolean column: CREATE/INSERT/SELECT of false/true/NULL, boolean×boolean
  // comparison and ORDER BY. boolean is also keyable — a boolean PRIMARY KEY / index uses the
  // bool-byte key encoding (the second non-integer key after uuid, encoding.md §2.9); casts
  // deferred (spec/design/types.md §9).
  "types.boolean_storable",
  // decimal / numeric scalar type — exact base-10, the first parameterized type
  // (numeric(p,s)), comparison/ordering/casts/storage + arithmetic. A valid PRIMARY KEY /
  // ordered index / UNIQUE key via the scale-independent decimal-order-preserving encoding
  // (encoding.md §2.5).
  "types.decimal",
  "expr.decimal_arithmetic",
  // exact-numeric transcendentals over decimal/numeric — sqrt/ln/exp/log/log10/power, a
  // PG-faithful arbitrary-precision port (numeric.c), byte-identical cross-core (decimal.md §8).
  "expr.numeric_transcendental",
  // bytea scalar type (variable-width raw bytes): storage, hex-input literals, and
  // unsigned-byte comparison/ordering. bytea is ALSO a key type — a bytea PRIMARY KEY / index /
  // UNIQUE uses the variable-width bytea-terminated-escape key encoding (encoding.md §2.6).
  "types.bytea",
  // uuid scalar type (fixed 16-byte RFC 4122): storage, PG-flexible input literals, and
  // unsigned-byte comparison/ordering. The FIRST non-integer type usable as a PRIMARY KEY.
  "types.uuid",
  // timestamp / timestamptz datetime types (i64 microseconds, instant model, no time zone
  // db): storage, literals (offset→UTC for tz), comparison/ordering, infinity, and a
  // timestamp PRIMARY KEY (key encoding = i64). spec/design/timestamp.md.
  "types.timestamp",
  "types.timestamptz",
  // interval scalar type (a span — months/days/micros): the "unit + time" input subset, PG
  // render, and comparison/ordering/dedup by the canonical 128-bit span. Non-key column only.
  // spec/design/interval.md.
  "types.interval",
  // f64/f32 (IEEE binary): storage, total order, kernel, casts, canonical-fold
  // SUM/AVG; exempt from cross-core identity for computed/rendered values (R tag). float.md.
  "types.f64",
  "types.f32",
  // float (f32/f64) as a KEY: PK / ordered index / UNIQUE / FK target + float-element array
  // keys, via the float-order-preserving encoding (encoding.md §2.8). float_key.test.
  "types.float_key",
  // date scalar type (a calendar date — i32 days since 1970-01-01): ISO literals, BC era,
  // infinity sentinels, comparison/ordering, a date PRIMARY KEY (key encoding = i32). A
  // strict island — no compare/cast to timestamp this slice. spec/design/date.md.
  "types.date",
  // interval ± interval → interval and unary minus (interval.md §5).
  "expr.interval_arithmetic",
  // interval ×÷ number → interval (the exact field-scaling cascade — interval.md §5).
  "expr.interval_scale",
  // timestamp[tz] ± interval and timestamp[tz] − timestamp[tz] → interval (interval.md §5).
  "expr.timestamp_arithmetic",
  // date ± integer → date, date − date → i32, date ± interval → timestamp (date.md §6).
  "expr.date_arithmetic",
  // General expression substrate — integer arithmetic, the boolean type, and the
  // AND/OR/NOT Kleene connectives (the `expression` profile).
  "types.boolean",
  "expr.arithmetic",
  "expr.unary_minus",
  "expr.parens",
  "expr.precedence",
  "expr.comparison_value",
  "expr.not_equal",
  "query.logical_connectives",
  "query.is_distinct_from",
  "error.division_by_zero",
  // Predicate forms (Phase 2, spec/design/grammar.md §20-§23).
  "expr.in_list",
  "expr.between",
  "expr.like",
  "expr.ilike",
  "expr.regex_match",
  "expr.regex_imatch",
  "expr.case",
  // Cost-accounting seam — the harness asserts the deterministic, cross-core-identical
  // accrued cost via the `# cost:` directive (CLAUDE.md §13).
  "resource.cost_metering",
  // Cost ceiling — a caller-set `max_cost` aborts a query (54P01) the instant accrued cost
  // reaches it; the `# max_cost:` directive runs a record under a ceiling (cost.md §6).
  "resource.cost_limit",
  // Nesting-depth limit — a fixed MAX_EXPR_DEPTH checked in the parser aborts deeply-nested input
  // with 54001 before it can overflow the native stack (CLAUDE.md §13; cost.md §7).
  "resource.depth_limit",
  // Input-size limit — a per-handle max_sql_length (default 1 MiB, 0 = unlimited) aborts an
  // over-long statement with 54000 at parse entry, before lexing; the `# max_sql_length:`
  // directive runs a record under a small cap (CLAUDE.md §13; cost.md §7, api.md §8).
  "resource.sql_length_limit",
  // Identifier-length limit — a fixed MAX_IDENTIFIER_LENGTH (63 bytes) checked at the lexer's
  // identifier production aborts an over-long name with 42622, on every parse path (cost.md §7).
  "resource.identifier_length_limit",
  // Composite-type nesting-depth limit — a fixed MAX_COMPOSITE_DEPTH (32) bounds the depth of a
  // composite-type chain at the producer: CREATE TYPE rejects an over-deep type with 54001, and a
  // loaded catalog that exceeds it is XX001, keeping every derived recursive walk (codec,
  // comparator, record_out/in, resolveColType) stack-safe (CLAUDE.md §13; cost.md §7b).
  "resource.composite_depth_limit",
  // Regex compiled-program size cap (MAX_REGEX_PROGRAM = 32768) — a well-formed but too-large
  // pattern aborts 54001 at compile, projectively, protecting the unlimited handle where the
  // regex_compile cost ceiling cannot (CLAUDE.md §13; cost.md §7c, regex.md §6).
  "resource.regex_program_limit",
  // Pure built-in surface — no function/operator or statement reaches the host (filesystem,
  // network, process, environment) or adds nondeterminism outside the entropy seam; escape-hatch
  // calls are 42883 and escape-hatch statements 42601 (CLAUDE.md §13; functions.md §13).
  "resource.pure_builtins",
  // Temp-table storage budget — temp_buffers bounds a session's RETAINED temporary-table bytes (the
  // hazard no cost ceiling covers); an over-budget temp write aborts 54P03. Measured in byte-identical
  // on-disk record bytes, checked per-statement, so the abort is cross-core-identical. The
  // # temp_buffers: directive sets the per-record budget (spec/design/temp-tables.md §7).
  "resource.temp_budget",
  // Shared-temp storage budget — shared_temp_mem bounds the GLOBAL shared temporary-table bytes (the
  // shared analogue of resource.temp_budget); an over-budget shared write aborts the same 54P03.
  // Measured identically (byte-identical on-disk record bytes), so cross-core-identical. The
  // # shared_temp_mem: directive sets the per-record budget (spec/design/temp-tables.md §7).
  "resource.shared_temp_budget",
  // Session privileges — the GRANT/REVOKE envelope (per-table SELECT/INSERT/UPDATE/DELETE + function
  // EXECUTE), enforced at name resolution with 42501; the # default_privileges: / # grant: /
  // # revoke: directives configure the session (session.md §5.3).
  "session.privileges",
  // DDL gate — the single allow_ddl session capability governing CREATE/DROP/ALTER; a denied schema
  // change is 42501. The # allow_ddl: directive sets it (session.md §5.3).
  "session.allow_ddl",
  // Temp-DDL gate — the temp-scoped split of allow_ddl: allow_temp_ddl governs CREATE/DROP of a
  // session-local temp table (42501 if denied), so a host can grant bounded scratch tables to an
  // untrusted session while withholding persistent DDL. The # allow_temp_ddl: directive sets it
  // (spec/design/temp-tables.md §5).
  "session.allow_temp_ddl",
  // Shared-temp-DDL gate — the shared-temp-scoped split of allow_ddl: allow_shared_temp_ddl governs
  // CREATE/DROP of a database-wide shared temp table (42501 if denied), independent of allow_ddl and
  // allow_temp_ddl. The # allow_shared_temp_ddl: directive sets it (temp-tables.md §5).
  "session.allow_shared_temp_ddl",
  // Session lifetime cost budget — a per-session cumulative cost budget lifetime_max_cost aborting the
  // in-flight statement (and rejecting later ones at admission) with 54P02 once the session's running
  // total reaches it; sibling to resource.cost_limit's per-statement 54P01. The sticky
  // # lifetime_max_cost: directive sets the budget for the rest of the file (session.md §5.4).
  "session.lifetime_cost",
  // Session variables — PostgreSQL's GUC model scoped to the session: a string→string map the host
  // sets (setVar/resetVar/var) and SQL reads with current_setting('name'[, missing_ok]). Custom
  // (dotted) names only; an unset name is 42704 unless missing_ok. The # set: directive configures
  // the session for the next record (session.md §6.1).
  "session.variables",
  // Phase 5 — explicit transactions: BEGIN/COMMIT/ROLLBACK, READ ONLY/READ WRITE access modes,
  // failed-block poisoning (spec/design/transactions.md §4, grammar.md §27).
  "txn.explicit",
  "txn.read_only",
  "txn.failed_state",
  // Shared-handle concurrency — the Database schedule format (spec/design/concurrency-testing.md
  // §4). Declared because this core implements Database + read/write Sessions + the watermark
  // (shared.ts); a core lacking them skips suites/concurrency files via the capability gate. This
  // core runs the schedule stepped-sequentially only (JS has no shared-memory threads for live
  // objects), which still defines + verifies the canonical, timing-free result (§4.3).
  "txn.shared",
  "txn.read_handle",
  "txn.watermark",
  // Layer 2 — the write-gate `blocks` annotation (spec/design/concurrency-testing.md §5). Declared
  // because this core defers a queued writer-open to the gate-releasing step (the canonical,
  // timing-free result). It has no stepped-threaded mode (JS cannot block one thread — write() while
  // a writer is open throws 25001, shared.ts), so it models the block by queuing rather than truly
  // blocking, which still defines + verifies the canonical result the threaded cores reproduce.
  "txn.gate_blocking",
  // The conformance harness can run a file against a PRE-BUILT database image named by a file-level
  // `# fixture:` directive (instead of a fresh DB), so the corpus can exercise on-disk state SQL
  // cannot construct — e.g. the version-skew read-safety regression (spec/design/collation.md
  // §12/§14, spec/design/conformance.md). Reconstructed in memory via loadEngine.
  "harness.fixture_open",
  // The `# upgrade-collations:` directive runs the COLLATION UPGRADE migration
  // (db.upgradeCollations) on the running DB — clears a version-skew so a corpus test can drive
  // skew→migrate→writable end to end (spec/design/collation.md §12).
  "harness.upgrade_collations",
];

// THIS FILE IS THE PUBLIC API of the TS core (CLAUDE.md §2 — the embedding surface): the converged
// Database/Session handles + the value/type/error/privilege/host-seam surface an embedder needs.
// Everything re-exported below is supported; the other src/*.ts modules are internal machinery
// (parser, AST, catalog, executor internals, codecs, storage, AND the low-level `Engine` handle) and
// are NOT part of the supported API even though TS cannot language-enforce that — import only from this
// barrel. Two sanctioned internal-seam exceptions: the in-repo harness/benches/tests import the
// low-level `Engine` from ./tooling.ts, and the browser worker (web/src/lib/jed/worker.ts) deep-imports
// executor.ts/opfs.ts/parser.ts directly to keep Node `fs` out of the bundle.

// --- primary embedding API ---
export { Database, Session } from "./shared.ts";
export { PreparedStatement, Rows, Transaction } from "./api.ts";
// The better-sqlite3-style ergonomic layer (spec/design/api.md §11): db.prepare(sql) → a Statement
// with run/get/all/iterate over native JS params + rows-as-objects. Additive — the raw Value[] path
// is unchanged.
export { Statement } from "./ergonomic.ts";
export type { JsParam, JsValue, Row, RunResult } from "./ergonomic.ts";
export { createDatabase, openDatabase } from "./file.ts";
export type { CreateOptions, OpenOptions } from "./file.ts";
export type { CollationInfo, Outcome, SessionOptions, TxStatus } from "./executor.ts";
export { intValue, nullValue, render } from "./value.ts";
export type { ThreeValued, Value } from "./value.ts";
export { EngineError, sqlStateCode } from "./errors.ts";
export type { SqlState } from "./errors.ts";
export { PrivilegeSet, Privileges, privilegeFromName } from "./privileges.ts";
export type { Privilege } from "./privileges.ts";
export { splitStatements } from "./split.ts";
export type { ScriptSummary, StatementSpan } from "./split.ts";
export { loadUnicodeData } from "./collation.ts";
export { advancingClock, fixedClock, seededRandomSource } from "./seam.ts";
export type { ClockFunc, RandomFill } from "./seam.ts";
export { DEFAULT_MAX_SQL_LENGTH, DEFAULT_PAGE_SIZE } from "./executor.ts";

// The low-level single-threaded `Engine` handle, its functional one-shot/transaction helpers
// (execute / query / prepare / begin / create / open / commit / close / …), and the golden/byte
// tooling (loadEngine / toImage) are deliberately NOT exported here — they live in ./tooling.ts, the
// in-repo internal seam. Embedders use the Database/Session surface above.
