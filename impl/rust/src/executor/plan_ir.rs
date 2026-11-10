//! The plan IR: `QueryPlan` and `SelectPlan` with their relations, joins and physical choices,
//! the set-returning-function and CTE plan nodes, and the scan-bound types an access path is
//! described by.

use super::*;

// ============================================================================
// Query plans — the resolved, owned form of a query, executable repeatedly (a correlated
// subquery is re-run once per outer row). `plan_query` (the resolve half of the old
// `run_select`) produces a `QueryPlan`; `exec_query_plan` (the execute half) consumes it
// against an outer-row environment. The split lets a subquery be resolved ONCE — so its
// structural/type errors surface even when the outer produces zero rows — yet executed many
// times (spec/design/grammar.md §26).
// ============================================================================

/// A resolved query expression: a SELECT plan or a set-operation plan (mirrors `QueryExpr`).
pub(crate) enum QueryPlan {
    /// Boxed to keep `QueryPlan` (and every `Result<QueryPlan>` on the recursive planning path —
    /// spec/design/cost.md §7.1) small.
    Select(Box<SelectPlan>),
    SetOp(Box<SetOpPlan>),
    /// A VALUES-body relation — `FROM (VALUES …) AS v` (spec/design/grammar.md §42): a computed
    /// relation of literal rows, the FROM-position sibling of `INSERT … VALUES`. Only ever produced
    /// as a derived-table body (the parser admits `VALUES` solely there), so it never appears as a
    /// set-op operand or a subquery operand.
    Values(ValuesPlan),
    /// A nested `WITH … query_expr` (spec/design/cte.md §7): the nested CTE bindings + their
    /// inline/materialize modes, and the inner query plan that references them. Establishes its own
    /// CTE scope at execution ([`exec_query_plan`] materializes the bindings and runs `body` against
    /// them). The output columns are the body's. Boxed to keep `QueryPlan` small.
    With(Box<WithPlan>),
}

/// A planned nested `WITH … query_expr` (spec/design/cte.md §7). `bindings` are the nested CTEs,
/// `modes` their per-binding inline/materialize decision ([`cte_modes`]), and `body` the inner query.
/// `inherited_len` pins the enclosing binding prefix visible at this exact plan position; execution
/// layers the local bindings over that prefix.
pub(crate) struct WithPlan {
    pub(crate) bindings: Vec<CteBinding>,
    pub(crate) modes: Vec<CteMode>,
    pub(crate) body: QueryPlan,
    pub(crate) inherited_len: usize,
}

impl WithPlan {
    /// Every planned query of this node's own CTE bindings — each body and, for a recursive CTE, its
    /// recursive term. They are planned against the same scope as the body (spec/design/cte.md §7),
    /// so an outer reference in one sits in the node's own correlation frame.
    pub(crate) fn binding_plans(&self) -> impl Iterator<Item = &QueryPlan> {
        self.bindings.iter().flat_map(|b| {
            let body = match &b.source {
                CteSource::Query(plan) => Some(plan),
                CteSource::Dml(_) => None,
            };
            body.into_iter()
                .chain(b.recursive.as_ref().map(|rt| &rt.plan))
        })
    }
}

impl QueryPlan {
    /// The output column types — for a scalar/IN subquery's plan-time column-count check (42601)
    /// and its folded/element type.
    pub(crate) fn column_types(&self) -> &[ResolvedType] {
        match self {
            QueryPlan::Select(s) => &s.column_types,
            QueryPlan::SetOp(s) => &s.column_types,
            QueryPlan::Values(v) => &v.column_types,
            QueryPlan::With(w) => w.body.column_types(),
        }
    }

    /// The output column names — the basis for a CTE's synthetic relation when there is no
    /// column-rename list (spec/design/cte.md §1).
    pub(crate) fn column_names(&self) -> &[String] {
        match self {
            QueryPlan::Select(s) => &s.column_names,
            QueryPlan::SetOp(s) => &s.column_names,
            QueryPlan::Values(v) => &v.column_names,
            QueryPlan::With(w) => w.body.column_names(),
        }
    }

    /// The per-output-column collation derivations (spec/design/collation.md §1 "Derived
    /// relations" / "Set operations") — the basis for a derived table / CTE synthetic column's
    /// collation and a set operation's common collation.
    pub(crate) fn column_derivs(&self) -> &[Deriv] {
        match self {
            QueryPlan::Select(s) => &s.column_derivs,
            QueryPlan::SetOp(s) => &s.column_derivs,
            QueryPlan::Values(v) => &v.column_derivs,
            QueryPlan::With(w) => w.body.column_derivs(),
        }
    }

    /// The per-output-column typmods (spec/design/create-table-as.md §2) — what a derived table /
    /// CTE synthetic column records and a `CREATE TABLE … AS` column keeps.
    pub(crate) fn column_typmods(&self) -> &[Typmod] {
        match self {
            QueryPlan::Select(s) => &s.column_typmods,
            QueryPlan::SetOp(s) => &s.column_typmods,
            QueryPlan::Values(v) => &v.column_typmods,
            QueryPlan::With(w) => w.body.column_typmods(),
        }
    }
}

/// A resolved VALUES-body relation (spec/design/grammar.md §42), executable to its literal rows.
/// `rows` is the resolved value expressions — `rows[r][c]` is row `r`, column `c` — each resolved
/// against no FROM row (a non-`LATERAL` body sees no sibling; it may correlate to an enclosing query
/// level, §42).
/// `column_types` is the per-column type unified across the rows like a set operation (§25), and
/// `column_names` is `column1, column2, …` (PostgreSQL; the derived table's optional column-rename
/// list overrides them at the synthetic relation). All rows have `column_types.len()` values.
pub(crate) struct ValuesPlan {
    pub(crate) rows: Vec<Vec<RExpr>>,
    pub(crate) column_types: Vec<ResolvedType>,
    pub(crate) column_names: Vec<String>,
    /// Each column's common collation across the rows (spec/design/collation.md §1), as for a
    /// UNION ALL column — a VALUES list never errors on an implicit conflict.
    pub(crate) column_derivs: Vec<Deriv>,
    /// Each column's common typmod across the rows (create-table-as.md §2).
    pub(crate) column_typmods: Vec<Typmod>,
}

/// One relation in a SELECT plan: the table name (looked up in the store at exec), the flat
/// offset of its first column in the joined row, and its column count (for NULL-padding). When
/// `srf` is `Some`, the relation is a COMPUTED set-returning function (generate_series) rather
/// than a base table: `table_name` is then the function name (never looked up in the store) and
/// the executor generates the rows instead of scanning (spec/design/functions.md §10).
pub(crate) struct PlanRel {
    pub(crate) table_name: String,
    /// The relation's explicit database qualifier (attached-databases.md §3), passed to the scope-aware
    /// store funnels at exec (`store_scoped` etc.). `None` for a bare implicit-scope name → the funnels
    /// fall through to the temp-first walk (behavior-neutral for every unqualified query).
    pub(crate) db: Option<String>,
    pub(crate) offset: usize,
    pub(crate) col_count: usize,
    pub(crate) srf: Option<SrfPlan>,
    /// When `Some(i)`, this relation is a reference to common-table-expression `i` (the index into
    /// the statement's CTE list — spec/design/cte.md), not a base table: `table_name` is then the
    /// CTE name (never looked up in the store) and the executor delivers its rows from the
    /// per-statement `CteCtx` (a materialized buffer, or the inlined body run in place).
    pub(crate) cte: Option<usize>,
    /// When `Some(plan)`, this relation is a DERIVED TABLE — `FROM (SELECT …) AS t`
    /// (spec/design/grammar.md §42): a parenthesized subquery used as a relation, mechanically an
    /// anonymous always-inlined single-reference CTE. `table_name` is the alias (never looked up in
    /// the store); the executor runs `plan` in place, charging its intrinsic cost — no `cte_scan_row`.
    /// Non-lateral (`lateral = false`) it was planned against the enclosing query's scope and reads
    /// this query's outer rows as its own (§42); a lateral one reads the left-hand row.
    pub(crate) derived: Option<Box<QueryPlan>>,
    /// When true, this relation is a CORRELATED `LATERAL` item (spec/design/grammar.md §44): its
    /// derived body / SRF args reference an earlier sibling (or an enclosing query), so the executor
    /// re-materializes it ONCE PER combined left-hand row (with that row pushed as its immediate outer
    /// — the correlated-subquery mechanism), rather than materializing it once. Always `false` for the
    /// first relation. Only a `srf` or `derived` relation is ever lateral.
    pub(crate) lateral: bool,
}

impl PlanRel {
    /// How many correlation frames this relation's derived body sits below its query: 1 for a
    /// correlated LATERAL body, planned against the prefix scope of earlier siblings
    /// (spec/design/grammar.md §44); 0 otherwise — a non-LATERAL body is planned against the
    /// enclosing query's scope, and an uncorrelated LATERAL body holds no outer reference (§42).
    pub(crate) fn derived_body_frame(&self) -> usize {
        usize::from(self.lateral)
    }
}

/// How a referenced CTE is evaluated (spec/design/cte.md §3, cost.md §3). Decided per CTE from its
/// reference count and `[NOT] MATERIALIZED` hint: a single-reference CTE is `Inline`, a
/// multi-reference (or `MATERIALIZED`) one is `Materialize`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CteMode {
    /// Run the body in place at each reference (re-evaluates per outer row under correlation,
    /// matching PostgreSQL); charges the body's intrinsic cost, no `cte_scan_row`.
    Inline,
    /// Run the body once, buffer the rows; each reference scans the buffer, charging `cte_scan_row`
    /// per buffered row.
    Materialize,
}

/// The per-statement CTE execution context, threaded through `exec_*` and `EvalEnv` so a FROM
/// reference (any nesting depth) can deliver a CTE's rows (spec/design/cte.md §5). `modes` and
/// `bindings` are fixed after planning; `buffers` is filled before the main query runs — one slot
/// per CTE in list order, holding the materialized rows of a `Materialize` CTE (an empty
/// placeholder for an `Inline` one, whose body is run in place from `bindings[ci].source` instead).
/// `bindings` also serves a data-modifying CTE's own inner queries, which resolve against the
/// earlier bindings when the writable-CTE orchestrator executes them (writable-cte.md §2).
/// `outers` holds, per binding, the outer-row environment of the `WITH` node that declared it: a
/// nested `WITH`'s CTE bodies are planned against the scope enclosing that node (cte.md §7), so an
/// inlined body run at a deeper reference site still reads that node's frames. Top-level bindings
/// have an empty environment.
#[derive(Clone, Copy)]
pub(crate) struct CteCtx<'a> {
    pub(crate) modes: &'a [CteMode],
    pub(crate) bindings: &'a [&'a CteBinding],
    pub(crate) buffers: &'a [&'a [Row]],
    pub(crate) outers: &'a [&'a [&'a [Value]]],
}

impl<'a> CteCtx<'a> {
    /// The empty context — no CTEs in scope (every non-`WITH` execution path).
    pub(crate) fn empty() -> CteCtx<'static> {
        CteCtx {
            modes: &[],
            bindings: &[],
            buffers: &[],
            outers: &[],
        }
    }

    /// The exact enclosing prefix visible where a nested WITH was planned. A CTE body can execute
    /// later against the statement's full context; truncation prevents later siblings leaking in.
    pub(crate) fn prefix(self, len: usize) -> CteCtx<'a> {
        assert!(len <= self.bindings.len(), "invalid inherited CTE prefix");
        CteCtx {
            modes: &self.modes[..len],
            bindings: &self.bindings[..len],
            buffers: &self.buffers[..len],
            outers: &self.outers[..len],
        }
    }
}

/// Owned slice headers for a flattened CTE execution context. Binding/buffer data stays borrowed;
/// only the small reference/mode vectors are composed when entering a nested WITH or a CTE prefix.
pub(crate) struct CteCtxView<'a> {
    pub(crate) modes: Vec<CteMode>,
    pub(crate) bindings: Vec<&'a CteBinding>,
    pub(crate) buffers: Vec<&'a [Row]>,
    pub(crate) outers: Vec<&'a [&'a [Value]]>,
}

impl<'a> CteCtxView<'a> {
    /// Layer top-level bindings (no enclosing outer-row environment) over `inherited`.
    pub(crate) fn extend(
        inherited: CteCtx<'a>,
        modes: &'a [CteMode],
        bindings: &'a [CteBinding],
        buffers: &'a [Vec<Row>],
    ) -> CteCtxView<'a> {
        CteCtxView::extend_at(inherited, &[], modes, bindings, buffers)
    }

    /// Layer a `WITH` node's bindings over `inherited`, recording `outer` — the node's outer-row
    /// environment — as each new binding's body frame (spec/design/cte.md §7).
    pub(crate) fn extend_at(
        inherited: CteCtx<'a>,
        outer: &'a [&'a [Value]],
        modes: &'a [CteMode],
        bindings: &'a [CteBinding],
        buffers: &'a [Vec<Row>],
    ) -> CteCtxView<'a> {
        let mut view = CteCtxView {
            modes: Vec::with_capacity(inherited.modes.len() + modes.len()),
            bindings: Vec::with_capacity(inherited.bindings.len() + bindings.len()),
            buffers: Vec::with_capacity(inherited.buffers.len() + buffers.len()),
            outers: Vec::with_capacity(inherited.outers.len() + bindings.len()),
        };
        view.modes.extend_from_slice(inherited.modes);
        view.modes.extend_from_slice(modes);
        view.bindings.extend_from_slice(inherited.bindings);
        view.bindings.extend(bindings.iter());
        view.buffers.extend_from_slice(inherited.buffers);
        view.buffers.extend(buffers.iter().map(Vec::as_slice));
        view.outers.extend_from_slice(inherited.outers);
        view.outers
            .extend(std::iter::repeat_n(outer, bindings.len()));
        view
    }

    pub(crate) fn ctx(&self) -> CteCtx<'_> {
        CteCtx {
            modes: &self.modes,
            bindings: &self.bindings,
            buffers: &self.buffers,
            outers: &self.outers,
        }
    }
}

/// Which set-returning function a [`SrfPlan`] is, selecting the row generator at exec
/// (spec/design/functions.md §10, array-functions.md §9). The dispatch is hand-written per core;
/// the resolution narrows the catalog name to one of these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SrfKind {
    /// `generate_series(start, stop[, step])` — an integer series (functions.md §10).
    GenerateSeries,
    /// `unnest(anyarray)` — one row per array element, flattened row-major (array-functions.md §9).
    Unnest,
    /// `unnest(record[]) AS u(c type, …)` — one row per anonymous-record element, its fields spread
    /// over the column-definition list's columns (the SrfPlan's `record_cols`; composite.md §13).
    UnnestRecord,
    /// `jsonb_array_elements(jsonb)` — one `jsonb` row per array element (json-sql-functions.md §3).
    JsonbArrayElements,
    /// `jsonb_array_elements_text(jsonb)` — one `text` row per array element (the `->>`-style render).
    JsonbArrayElementsText,
    /// `jsonb_object_keys(jsonb)` — one `text` row per object key, in canonical key order.
    JsonbObjectKeys,
    /// `json_object_keys(json)` — one `text` row per object key, in INPUT order (duplicates kept).
    JsonObjectKeys,
    /// `jsonb_each(jsonb)` — one `(key text, value jsonb)` row per top-level object member, canonical
    /// key order (json-sql-functions.md §3). A two-column SRF (the C0 multi-column synthetic table).
    JsonbEach,
    /// `jsonb_each_text(jsonb)` — one `(key text, value text)` row per member (the `->>`-style value).
    JsonbEachText,
    /// `json_array_elements(json)` — one `json` row per array element, its VERBATIM sub-text (json.md
    /// §4: whitespace, key order, duplicate keys kept).
    JsonArrayElements,
    /// `json_array_elements_text(json)` — one `text` row per element (the `->>`-style render).
    JsonArrayElementsText,
    /// `json_each(json)` — one `(key text, value json)` row per top-level member in INPUT order
    /// (duplicate keys kept), the value its verbatim sub-text.
    JsonEach,
    /// `json_each_text(json)` — one `(key text, value text)` row per member (the `->>`-style value).
    JsonEachText,
    /// `json[b]_to_record` / `json[b]_to_recordset` (R1, json-table.md §2): map a JSON object's members
    /// to the C0 col-def-list columns by name, coercing each to its declared type. `set` = the
    /// recordset form (one row per array element); else one record row. `jsonb` selects the input type.
    JsonRecord { jsonb: bool, set: bool },
    /// `regexp_matches(string, pattern [, flags])` (regex.md §8c): one `text[]` row per match (at most
    /// one without the `g` flag). `args` is the single [`RExpr::RegexFunc`] call node.
    RegexpMatches,
    /// `regexp_split_to_table(string, pattern [, flags])` (regex.md §8c): one `text` row per split
    /// piece. `args` is the single [`RExpr::RegexFunc`] call node.
    RegexpSplitToTable,
    /// `jsonb_path_query(jsonb, jsonpath)` (P2, jsonpath.md §5.2): one `jsonb` row per item of the
    /// path's evaluation sequence over the context document. `args` is `[ctx, path]`.
    JsonbPathQuery,
    /// `JSON_TABLE(ctx, path COLUMNS (…))` (T1, json-table.md §3): a multi-column relation produced by
    /// the recursive default-plan expansion. `args` is `[ctx]`; the resolved column tree is the
    /// SrfPlan's `json_table` field.
    JsonTable,
    /// The `jed_tables` catalog relation (spec/design/introspection.md §5): a read-only COMPUTED
    /// relation — one row per user table of the qualified database, derived at execution from its
    /// pinned catalog snapshot. Not a function (it is resolved as a table name), but it rides the
    /// SRF plan shape so every "computed, not scanned" gate handles it: no store, no index
    /// pushdown, no PK order, excluded from the fast-path lanes. `args` is empty; the scope is the
    /// SrfPlan's `introspect_scope`.
    JedTables,
    /// The `jed_columns` catalog relation (introspection.md §5) — one row per column of every user
    /// table of the qualified database, in (table, ordinal) order.
    JedColumns,
    /// The `jed_indexes` catalog relation (introspection.md §5.1, slice I2) — one row per secondary
    /// index of every user table of the qualified database (name, table, columns, is_unique,
    /// method), in (table, index-name) order.
    JedIndexes,
    /// The `jed_constraints` catalog relation (introspection.md §5.1, slice I2) — one row per
    /// CHECK / UNIQUE / FK / EXCLUDE constraint of every user table, in (table, kind, name) order.
    JedConstraints,
    /// The P9 one-row-per-analyzed-column statistics summary relation.
    JedStatistics,
    /// The `jed_sequences` catalog relation (introspection.md §5.3, slice I3) — one row per
    /// sequence of the qualified database.
    JedSequences,
    /// The `jed_types` catalog relation (introspection.md §5.3, slice I3) — one row per
    /// user-defined type of the qualified database.
    JedTypes,
    /// The `jed_type_fields` catalog relation (introspection.md §5.3, slice I3) — one row per field
    /// of every user-defined type, in (type, ordinal) order.
    JedTypeFields,
    /// The `jed_views` catalog relation (introspection.md §5.4, views V1) — one row per view of the
    /// qualified database, in ascending name byte order.
    JedViews,
}

impl SrfKind {
    /// Whether the kind is a built-in catalog relation (introspection.md §5) rather than a
    /// set-returning function: EXPLAIN renders it as a `Catalog Scan` of its database scope.
    pub(crate) fn is_catalog(&self) -> bool {
        matches!(
            self,
            SrfKind::JedTables
                | SrfKind::JedColumns
                | SrfKind::JedIndexes
                | SrfKind::JedConstraints
                | SrfKind::JedStatistics
                | SrfKind::JedSequences
                | SrfKind::JedTypes
                | SrfKind::JedTypeFields
                | SrfKind::JedViews
        )
    }
}

/// A resolved `JSON_TABLE` plan (T1, json-table.md §3) — the compiled root path + the column tree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct JtPlan {
    /// The compiled root jsonpath (its evaluation over `ctx` yields the row items).
    pub(crate) root_path: String,
    /// The `PASSING` variable names; their values are the SRF's arguments after the context.
    pub(crate) passing_names: Vec<String>,
    /// The total number of flattened output columns.
    pub(crate) width: usize,
    /// The top-level column tree.
    pub(crate) columns: Vec<JtCol>,
}

/// One resolved `JSON_TABLE` column (json-table.md §3.3). Leaf columns carry their flat output index.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum JtCol {
    /// `FOR ORDINALITY` — the level's 1-based row counter.
    Ordinality { idx: usize },
    /// A regular column: evaluate `path` over the row item, apply JSON_VALUE (scalar) or JSON_QUERY
    /// (json/jsonb) semantics, coerce to `returning`.
    Regular {
        idx: usize,
        returning: ScalarType,
        decimal: Option<DecimalTypmod>,
        path: String,
        /// JSON_QUERY semantics (json/jsonb returning) vs JSON_VALUE (scalar).
        query: bool,
        wrapper: JsonWrapper,
        on_empty: JsonOnBehavior,
        on_error: JsonOnBehavior,
        /// The index of the `DEFAULT expr` of `on_empty` / `on_error` among the plan's default
        /// expressions (json-sql-functions.md §5.3), which follow the context and the `PASSING` values
        /// in the SRF's arguments.
        on_empty_default: Option<usize>,
        on_error_default: Option<usize>,
    },
    /// An `EXISTS` column: JSON_EXISTS of `path`, coerced to `returning` (bool/int).
    Exists {
        idx: usize,
        returning: ScalarType,
        path: String,
        on_error: JsonOnBehavior,
    },
    /// A `NESTED PATH` subtree: expanded over the row item (the default-plan LEFT OUTER / sibling UNION).
    Nested { path: String, columns: Vec<JtCol> },
}

/// A resolved set-returning-function row source (spec/design/functions.md §10, array-functions.md
/// §9). `kind` selects the generator: `generate_series(start, stop[, step])` (`args` = 2 or 3
/// integers) or `unnest(anyarray)` (`args` = the single array expression). Non-LATERAL, so each
/// arg evaluates against the params/outer environment with no local row. The produced column's
/// type lives on the synthetic relation (built in `resolve_srf`), so the plan needs only the
/// resolved arg expressions here.
pub(crate) struct SrfPlan {
    pub(crate) kind: SrfKind,
    pub(crate) args: Vec<RExpr>,
    /// The declared output columns for a record-returning SRF (`JsonRecord`) — the C0 col-def list,
    /// used to map JSON members to columns by name + coerce. Empty for every other SRF kind.
    pub(crate) record_cols: Vec<Column>,
    /// The resolved column tree for a `JSON_TABLE` SRF (`JsonTable`), else `None`.
    pub(crate) json_table: Option<Box<JtPlan>>,
    /// The validated database scope of a catalog relation (`JedTables` / `JedColumns` —
    /// introspection.md §5): `"main"` (also the unqualified default), `"temp"`, or an exact
    /// attachment name. Empty for every other kind.
    pub(crate) introspect_scope: String,
}

/// One join in a SELECT plan: its kind and resolved ON predicate (`None` for CROSS). The right
/// relation is `rels[k+1]`.
pub(crate) struct PlanJoin {
    pub(crate) kind: JoinKind,
    pub(crate) on: Option<RExpr>,
}

/// A resolved SELECT, executable against an outer-row environment (the execute half of the old
/// `run_select`, lifted to a value so a correlated subquery can re-run it per outer row).
/// One resolved grouping set of a `GROUP BY` (spec/design/aggregates.md §12). For a plain `GROUP BY`
/// there is exactly one of these; `ROLLUP`/`CUBE`/`GROUPING SETS` produce several. Each is bucketed
/// independently over the post-`WHERE` rows and its groups projected into the shared synthetic row,
/// whose first `group_keys.len()` slots are the *master* grouping columns (the ordered union of all
/// sets' columns).
pub(crate) struct GroupSetPlan {
    /// The flat input-row indices this set buckets on (its key, in key order). Empty = one grand-total
    /// group (always emits one row, even over an empty input — the `()` / whole-table case).
    pub(crate) key_cols: Vec<usize>,
    /// Per master-grouping-column slot (length `group_keys.len()`): `Some(j)` if this set includes
    /// that column — its synthetic value is the bucket key's `j`-th component — else `None`, meaning
    /// the column is not grouped in this set and its synthetic value is `NULL`.
    pub(crate) slot_src: Vec<Option<usize>>,
    /// The `GROUPING()` bitmask for rows from this set: bit `p` is set iff master slot `p` is NOT in
    /// this set (so `GROUPING(col)` returns 1 for a column grouped away in this set).
    pub(crate) mask: i64,
}

pub(crate) struct SelectPlan {
    pub(crate) rels: Vec<PlanRel>,
    pub(crate) joins: Vec<PlanJoin>,
    /// The complete WHERE (`filter.get()`), owned together with the stage-3 access predicate built
    /// around it when an ON conjunct is scan-pushed (`filter.access()`; rewrite.rs `PlanFilter`).
    pub(crate) filter: PlanFilter,
    pub(crate) is_agg: bool,
    pub(crate) group_keys: Vec<usize>,
    /// The materialized general-expression `GROUP BY` keys (`GROUP BY a + b`, aggregates.md §15), in
    /// synthetic-slot order. Before bucketing, each post-WHERE row evaluates these and appends the
    /// values at flat slots `input_width + k`, so a master grouping key index in `group_keys` /
    /// `group_sets` may point at one — the whole-row bucket machinery stays slot-based. Empty when
    /// every grouping key is a plain column (the common case, byte-identical to before).
    pub(crate) group_exprs: Vec<RExpr>,
    /// The grouping sets to compute (spec/design/aggregates.md §12). A plain `GROUP BY` (and the
    /// whole-table aggregate) is a single set; `ROLLUP`/`CUBE`/`GROUPING SETS` produce several.
    pub(crate) group_sets: Vec<GroupSetPlan>,
    /// One entry per `GROUPING(...)` call in the projection / HAVING, in synthetic-slot order: the
    /// master-grouping-column positions of its arguments. Each call's value per group row is computed
    /// from the row's grouping-set `mask` and appended after the aggregate results.
    pub(crate) grouping_specs: Vec<Vec<usize>>,
    pub(crate) agg_specs: Vec<AggSpec>,
    /// `true` when the select list has a window function — the query runs the blocking WINDOW
    /// stage (after WHERE, before ORDER BY/LIMIT) and takes the eager path (never streaming).
    /// Mutually exclusive with `is_agg` in S0 (spec/design/window.md §5.2).
    pub(crate) has_window: bool,
    /// One resolved window function per select-list `OVER` call (empty unless `has_window`). The
    /// window stage appends each spec's per-row result after the input columns and the materialized
    /// window keys, so the projection references result `i` as flat slot
    /// `input_width + window_keys.len() + i` (spec/design/window.md §5.1).
    pub(crate) window_specs: Vec<WindowSpec>,
    /// The materialized window-key expressions (a non-column PARTITION BY / ORDER BY key —
    /// `PARTITION BY a + b`, `ORDER BY a % 2`), in synthetic-slot order. Before the window stage each
    /// row evaluates these and appends the values at flat slots `input_width + k`, so the partition /
    /// sort / frame machinery (which is slot-based) is unchanged (spec/design/window.md §5.1). Empty
    /// when every window key is a bare column (the common case, byte-identical to before).
    pub(crate) window_keys: Vec<RExpr>,
    pub(crate) having: Option<RExpr>,
    /// (flat slot, descending, nulls_first, collation) per ORDER BY key. A column / ordinal key's slot
    /// is its real input / grouped-row slot; a general-**expression** key's slot is `final_row_width +
    /// k`, indexing the k-th materialized order value appended to the pre-sort row (grammar.md §10).
    pub(crate) order: Vec<SortKey>,
    /// The materialized `ORDER BY` expression-key expressions (`ORDER BY a + 1`, `ORDER BY abs(b)`), in
    /// the order their sort slots reference them. Just before the sort each row evaluates these and
    /// appends the values at `final_row_width + k` (after any window / grouped columns), so the
    /// slot-based sort stays unchanged — the window-key precedent (window.md §5.1). Empty when every
    /// ORDER BY key is a bare column or ordinal (the common case, byte-identical to before).
    pub(crate) order_exprs: Vec<RExpr>,
    pub(crate) projections: Vec<RExpr>,
    pub(crate) column_names: Vec<String>,
    pub(crate) column_types: Vec<ResolvedType>,
    /// Each output column's collation derivation (collation.md §1 "Derived relations"), carried
    /// into a derived table / CTE over this SELECT.
    pub(crate) column_derivs: Vec<Deriv>,
    /// Each output column's typmod (create-table-as.md §2), carried the same way.
    pub(crate) column_typmods: Vec<Typmod>,
    pub(crate) distinct: bool,
    /// `DISTINCT ON`: the number of leading `order` keys that form the dedup key (0 = none). The
    /// sorted rows keep the first of each run of equal keys, before LIMIT/OFFSET and projection
    /// (spec/design/grammar.md §11.1). `distinct` is false when this is set.
    pub(crate) distinct_on: usize,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
    /// The **touched set** per relation (cost.md §3 "The touched set"; large-values.md §14): which
    /// of its columns this query statically references. Drives the chain-`page_read` /
    /// `value_decompress` portion of the scan's up-front cost block — an untouched spilled or
    /// compressed column charges nothing, however many records the bound admits. An ANNOTATION of
    /// the logical plan, not an optimization: a wrong mask is a disk-mode NULL-folding correctness
    /// bug, not a slow plan — so it is computed by the resolve half (`compute_rel_masks`), never by
    /// a physical rule (spec/design/planner.md §2).
    pub(crate) rel_masks: Vec<Vec<bool>>,
    /// The stage-2 WHERE rewrite (rewrite.rs; spec/design/planner.md §3): a proven contradiction,
    /// or the per-relation pushed filters plus the post-join residual. `None` when no rewrite fired.
    /// `filter` above stays the COMPLETE WHERE — the input every access-path detector and estimator
    /// rule reads; execution evaluates `post_join_filter()` over joined rows.
    pub(crate) pushdown: Option<WherePushdown>,
    /// The output layout of an unaliased parenthesized join's body (planner.rs
    /// `join_group_outputs`; spec/design/grammar.md §15); `None` for every written SELECT.
    pub(crate) group: Option<JoinGroupLayout>,
    /// The plan's physical / access-path decisions — set ONLY by the `optimize_select` pass
    /// (optimize.rs); default (zero-valued) when resolve hands the plan over
    /// (spec/design/planner.md §4).
    pub(crate) phys: PhysicalPlan,
}

impl SelectPlan {
    /// The touched mask over the logical combined row (each relation's mask concatenated in column
    /// order) — how query memory measures a pre-projection row (spec/design/memory.md §3). Empty when
    /// the meter is unlimited, since no measurement will run.
    pub(crate) fn memory_mask(&self, meter: &Meter) -> Vec<bool> {
        if meter.query_memory_active() {
            self.rel_masks.concat()
        } else {
            Vec::new()
        }
    }
}

/// The physical/access-path half of a [`SelectPlan`]: every field is the output of one discrete
/// rule of the `optimize_select` pass (spec/design/planner.md §4), applied in a fixed order after
/// the resolve half has built the logical plan. A defaulted `PhysicalPlan` is always correct — the
/// executor then full-scans and eager-sorts.
#[derive(Default)]
pub(crate) struct PhysicalPlan {
    /// Physical join position -> logical FROM ordinal. P7 sets `[0, 1]` or `[1, 0]` for eligible
    /// two-base INNER/CROSS joins; empty retains source order at barriers. Resolved slots never move.
    pub(crate) relation_order: Vec<usize>,
    /// One entry per physical append step for P8's N-way all-base INNER/CROSS plan. The step owns
    /// the authored ON trees that become dependency-complete there, in source order, plus an
    /// optional deterministic hash operator. An INL step is identified by the appended relation's
    /// `rel_inl_bounds` entry; otherwise no hash means nested loop. Empty retains the legacy join
    /// tree (including every semantic barrier and P7's two-relation representation).
    pub(crate) join_steps: Vec<PhysicalJoinStep>,
    /// Deterministic two-input hash operator. Builds the right input and probes the left using
    /// same-type bare-column equality keys in source order. `None` keeps nested loop.
    pub(crate) hash_join: Option<HashJoinPlan>,
    /// `ORDER BY` is satisfied by the single base relation's **primary-key scan order** — the
    /// table tree already yields rows in this order, so the sort is elided (and with a `LIMIT`
    /// the scan short-circuits a top-N). True iff the query is a single-table, non-aggregate,
    /// non-`DISTINCT` `SELECT` whose `ORDER BY` keys are a prefix of the PK columns, all one
    /// direction, with the column's stored key collation (spec/design/cost.md §3 "ORDER BY
    /// satisfied by primary-key order"). Secondary-index order is a follow-on.
    pub(crate) pk_ordered: bool,
    /// The PK scan direction when `pk_ordered`: `true` ⇒ the order is all-`DESC` over the full PK,
    /// served by a **reverse** scan; `false` ⇒ all-`ASC` (forward). Always `false` when
    /// `!pk_ordered`.
    pub(crate) pk_reverse: bool,
    /// `ORDER BY` is satisfied by walking a **B-tree secondary index** in key order (with a `LIMIT`
    /// top-N) — `Some(index)` when the PK scan does not satisfy the order but the index does
    /// (cost.md §3 "secondary-index order"). Mutually exclusive with `pk_ordered` (the PK scan is
    /// cheaper). `None` keeps the eager/streaming sort.
    pub(crate) index_order: Option<IndexOrder>,
    /// `ORDER BY` is satisfied by the OUTER relation's primary-key scan order in a two-table
    /// INNER/CROSS join (cost.md §3 "JOIN"): the join drives/probes the outer in PK order, so its
    /// output is already in order — the sort is elided and a `LIMIT` short-circuits the loop. Set only
    /// for exactly two non-lateral base relations, a `LIMIT`, and a forward outer-PK `ORDER BY`.
    pub(crate) join_pk_ordered: bool,
    /// `K = OFFSET + LIMIT` for a blocking plain-SELECT sort. `None` means the rule did not fire
    /// (or K overflowed), so the ordinary full sort remains authoritative.
    pub(crate) top_k: Option<i64>,
    /// Scan-bound pushdown, **one entry per relation** in `rels`: the WHERE conjuncts that
    /// bound that relation's scan — a primary-key range, or (when no PK bound applies) a
    /// secondary-index equality (cost.md §3 "bounded scan" / "index-bounded scan"). `None` ⇒
    /// a full scan of that relation. In a JOIN each base table is bounded independently by
    /// the WHERE predicates against a CONSTANT (literal/param/outer) — a cross-relation
    /// `b.pk = a.x` is the index-nested-loop case (still a follow-on; `const_source` rejects
    /// a sibling column). The residual filter stays the WHOLE `filter`, re-applied after the
    /// join — the bound only narrows which rows are scanned.
    pub(crate) rel_bounds: Vec<Option<ScanBound>>,
    /// Deterministic base candidate estimates. P6b composes them into complete one-base-relation
    /// access/ordering pipelines; joins retain staged legacy policies. Execution never reads this.
    pub(crate) rel_estimates: Vec<Vec<crate::estimator::CandidateEstimate>>,
    /// **Index-nested-loop** scan bounds, one per relation (cost.md §3 "JOIN"). `Some` for a join
    /// inner relation whose primary key / indexed column is compared to a **sibling** column of an
    /// earlier relation (`a JOIN b ON b.pk = a.x`) — a `BoundSrc::Sibling` bound resolved per outer
    /// row from the combined left-hand row. When set, that relation is NOT materialized once up
    /// front; the join loop re-materializes it per left row (like a correlated `LATERAL`), seeking
    /// instead of full-scanning — O(N·M) → O(N·log M). `None` ⇒ the ordinary once-materialized
    /// `rel_bounds` path. A set entry takes precedence over `rel_bounds` for that relation.
    pub(crate) rel_inl_bounds: Vec<Option<ScanBound>>,
}

pub(crate) struct HashJoinPlan {
    pub(crate) keys: Vec<HashJoinKey>,
}

pub(crate) struct PhysicalJoinStep {
    pub(crate) on_indices: Vec<usize>,
    pub(crate) hash_join: Option<HashJoinPlan>,
}

pub(crate) struct HashJoinKey {
    pub(crate) left: usize,
    pub(crate) right: usize,
    pub(crate) ty: Type,
}

// ---- Primary-key predicate pushdown (spec/design/cost.md §3 "bounded scan / point lookup") ----
//
// A single-table WHERE on the primary key bounds the storage-key range a scan must visit. Detection
// is two-stage: `detect_pk_bound` at plan time (structural — which conjuncts are PK comparisons),
// `build_key_bound` at exec time (the const values, and any $N, are known only then). The bound is a
// SUPERSET of the matching keys: the whole WHERE stays the residual filter (re-applied to each scanned
// row), so the result is always correct — the bound only narrows which rows are scanned, and the
// page_read/storage_row_read drop to what it touches. The unbounded case keeps the full scan, so its
// cost never moves.

/// The constant data extracted from a bound term's const-source operand (avoids cloning the whole
/// `RExpr` and keeps `PkBound` owned). `Timestamp` covers both timestamp and timestamptz (same
/// encoding; the PK type disambiguates). `Param`, `Outer`, and `Sibling` resolve to a value at exec
/// time: `Param` from the bound parameters, `Outer` from an enclosing query's row (a correlated
/// reference — the inner subquery's PK is bounded by the current outer row's column, so it seeks
/// instead of re-scanning the whole inner table per outer row; spec/design/cost.md §3 "bounded
/// scan", grammar.md §26), and `Sibling` from the current combined (left/running) row of a
/// left-deep join — an EARLIER join relation's column, so the inner relation seeks per outer row
/// instead of full-scanning for every outer row (index-nested-loop join, cost.md §3 "JOIN"). The
/// `Sibling` source is the join analog of `Outer`: the same per-outer-row bound, resolved from the
/// sibling row rather than an enclosing query's row.
#[derive(Clone)]
pub(crate) enum BoundSrc {
    Int(i64),
    Bool(bool),
    Uuid([u8; 16]),
    Timestamp(i64),
    Date(i32),
    Text(String),
    Bytea(Vec<u8>),
    Decimal(Decimal),
    Float32(f32),
    Float64(f64),
    Interval(Interval),
    Null,
    Param(usize),
    Outer {
        level: usize,
        index: usize,
    },
    /// Index-nested-loop: the GLOBAL column index of an earlier join relation, read from the
    /// current combined left-hand row at exec time (cost.md §3 "JOIN"). Only ever produced by
    /// `detect_inl_bound` for a join inner relation; never appears in the once-materialized
    /// `rel_bounds`.
    Sibling(usize),
}

/// One `pk <op> const-source` from a WHERE AND-chain, normalized so the PK is the LEFT side (a
/// `5 < pk` flips to `pk > 5`).
#[derive(Clone)]
pub(crate) struct BoundTerm {
    pub(crate) op: CmpOp,
    pub(crate) src: BoundSrc,
}

pub(crate) struct PkEqCol {
    pub(crate) name: String,
    pub(crate) col_type: ScalarType,
    pub(crate) srcs: Vec<BoundSrc>,
    pub(crate) ranges: Vec<BoundTerm>,
    pub(crate) coll: Option<std::sync::Arc<Collation>>,
}

pub(crate) struct PkRange {
    pub(crate) name: String,
    pub(crate) col_type: ScalarType,
    pub(crate) terms: Vec<BoundTerm>,
    pub(crate) coll: Option<std::sync::Arc<Collation>>,
}

/// The plan-time result of PK tuple analysis: a maximal equality prefix plus an optional range on
/// the next member. The concrete key range is built per execution by `build_key_bound`.
pub(crate) struct PkBound {
    pub(crate) eq_cols: Vec<PkEqCol>,
    pub(crate) range: Option<PkRange>,
    pub(crate) member_count: usize,
}

/// One OR disjunct represented as the conjunction of its bound terms. It becomes one logical key
/// interval at execution, after params/outer sources are known. An array spec (`array` set, no
/// terms) is a `key = ANY(array)` disjunct: at execution it becomes one point per non-NULL element
/// of the array's value.
pub(crate) struct IntervalSpec {
    pub(crate) terms: Vec<BoundTerm>,
    pub(crate) array: Option<ArrayPointSource>,
}

/// A membership subquery's value list as a key's point source (cost.md §3 "OR / IN-list"): its single
/// column's values are points of the scalar left operand's type exactly (`elem_exact`) or after an
/// integer/decimal conversion (`elem_promoted`). The uncorrelated fold fills `values`; an
/// interval-set bound planned before the fold reads them through this shared cell.
pub(crate) struct FoldedPoints {
    pub(crate) values: std::sync::OnceLock<Vec<Value>>,
    pub(crate) elem_exact: bool,
    pub(crate) elem_promoted: bool,
}

/// The array operand of a `key = ANY(array)` interval-set disjunct (cost.md §3 "OR / IN-list") —
/// constant for the whole scan and readable without evaluation — in the owned form the plan keeps
/// (the `BoundSrc` precedent).
pub(crate) enum ArrayPointSource {
    /// A one-dimensional `ARRAY[...]` constructor: each element a constant source of the key.
    Elements(Vec<BoundSrc>),
    /// A bind parameter's array value.
    Param(usize),
    /// A correlated outer column's array value (the `BoundSrc::Outer` indexing).
    Outer { level: usize, index: usize },
    /// A folded array constant.
    Const(Box<ArrayVal>),
    /// A NULL array: no points.
    Null,
    /// An uncorrelated `IN (SELECT …)` / `= ANY(SELECT …)`: its value list once folded.
    Folded(std::sync::Arc<FoldedPoints>),
    /// A folded `IN` list a mutation plans over.
    List(Vec<Value>),
    /// Index-nested-loop: a sibling array column, read from the current combined left-hand row by
    /// its GLOBAL column index (the `BoundSrc::Sibling` precedent).
    Sibling(usize),
}

/// Canonical interval-set plan over a single-column PK. Specs are OR disjuncts; `clip` is the
/// co-present top-level AND bounds on that key.
///
/// A composite PK instead carries `tuple` (cost.md §3 "Composite primary keys"): the consumed
/// members in key order, every one but possibly the last a point member. `pk_type`/`coll`/`specs`/
/// `clip` are then unused.
pub(crate) struct PkKeySet {
    pub(crate) pk_type: ScalarType,
    pub(crate) coll: Option<std::sync::Arc<Collation>>,
    pub(crate) specs: Vec<IntervalSpec>,
    pub(crate) clip: Vec<BoundTerm>,
    pub(crate) tuple: Vec<TupleMember>,
    pub(crate) member_count: usize,
    pub(crate) all_fixed: bool,
}

/// One consumed member of a composite-PK tuple interval set. `specs` is the member's first reducing
/// OR/IN/ANY conjunct (`None` when it has none); `clip` is every direct comparison on it. `point`
/// marks a point member (a direct equality, or a set of pure equalities/arrays); a non-point member
/// is the final member, whose canonical intervals map under each prefix.
pub(crate) struct TupleMember {
    pub(crate) name: String,
    pub(crate) column: usize,
    pub(crate) col_type: ScalarType,
    pub(crate) coll: Option<std::sync::Arc<Collation>>,
    pub(crate) specs: Option<Vec<IntervalSpec>>,
    pub(crate) clip: Vec<BoundTerm>,
    pub(crate) point: bool,
}

/// The [`PkKeySet`] analog over a B-tree secondary index whose keys are plain columns (indexes.md
/// §5; cost.md §3 "Multi-column secondary indexes"): `tuple` is the consumed key columns in key
/// order, walked as a composite-PK tuple set's members, and `col_types` is every key column's type —
/// the per-entry key-suffix skip. A tuple of one member is the leading-column interval set.
/// `all_fixed` reports that every key column is fixed-width, so the probes merge as the PK's do.
pub(crate) struct IndexKeySet {
    pub(crate) name_key: String,
    pub(crate) tuple: Vec<TupleMember>,
    pub(crate) slots: Vec<KeySlot>,
    /// Every key column's sort order (indexes.md §11), parallel to `slots`: a member's points
    /// are its §2.16 present slots.
    pub(crate) orders: Vec<KeyOrder>,
    pub(crate) all_fixed: bool,
}

/// One canonical index interval with its row-key suffix recovery: the known prefix byte length
/// skipped first, then the self-delimiting member slots after it (indexes.md §5.1).
pub(crate) struct IndexProbe {
    pub(crate) bound: KeyBound,
    pub(crate) prefix_len: usize,
    pub(crate) suffix: Vec<KeySlot>,
}

/// A per-relation scan bound (cost.md §3): a primary-key range, a secondary-index
/// equality (spec/design/indexes.md §5), a GIN-bounded scan over an array column
/// (spec/design/gin.md §6), a GiST-bounded scan, or a canonical interval set. Candidate inventory
/// and consumer selection are deliberately separate; this remains the executor-facing shape of
/// the selected candidate.
pub(crate) enum ScanBound {
    Pk(PkBound),
    Index(IndexBound),
    Gin(GinBound),
    Gist(GistBound),
    PkSet(PkKeySet),
    IndexSet(IndexKeySet),
}

/// Small physical plan shared by UPDATE/DELETE execution and DML EXPLAIN. The resolved filter stays
/// outside as the residual predicate; `bound` is only the chosen candidate superset. `db` carries
/// the target qualifier so a full scan continues through the scoped store funnel.
pub(crate) struct MutationScanPlan {
    pub(crate) bound: Option<ScanBound>,
    pub(crate) db: Option<String>,
    /// The stage-2 proof (planner.md §3.1) that the WHERE is never TRUE: the bound is still
    /// selected (EXPLAIN renders it), but execution reads nothing.
    pub(crate) contradiction: bool,
}

/// Normalized result of executing any mutation access path: keyed rows plus the exact up-front
/// page/decompression units charged before per-row `storage_row_read`.
pub(crate) struct MutationScanBatch {
    pub(crate) entries: Vec<(Vec<u8>, Row)>,
    pub(crate) pages: usize,
    pub(crate) slabs: usize,
}
