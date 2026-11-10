//! The catalog relations (`jed_tables`, `jed_columns`, ...) and the synthetic tables that
//! set-returning functions in FROM are resolved against: their names, columns and aliases.

use super::*;

/// Classify a relation name as a built-in catalog relation (introspection.md §5): the `jed_`-prefixed
/// relation family, byte-exactly (a bare spelling is already folded; a quoted mixed-case spelling
/// is a different name, grammar.md §3). Built-in names resolve in every database's relation namespace, checked AFTER a
/// statement-local CTE (a CTE shadows a catalog relation — PG-matching, oracle-checked) and BEFORE
/// the user catalog (post-I0 the two can never collide; for a pre-reservation legacy file the
/// built-in wins and the user relation is unreachable by name — §5).
pub(crate) fn catalog_rel_kind(name: &str) -> Option<SrfKind> {
    match name {
        "jed_tables" => Some(SrfKind::JedTables),
        "jed_columns" => Some(SrfKind::JedColumns),
        "jed_indexes" => Some(SrfKind::JedIndexes),
        "jed_constraints" => Some(SrfKind::JedConstraints),
        "jed_statistics" => Some(SrfKind::JedStatistics),
        "jed_sequences" => Some(SrfKind::JedSequences),
        "jed_types" => Some(SrfKind::JedTypes),
        "jed_type_fields" => Some(SrfKind::JedTypeFields),
        "jed_views" => Some(SrfKind::JedViews),
        _ => None,
    }
}

/// Whether `name` is a built-in catalog relation (introspection.md §5). The write paths
/// use it to reject a catalog relation as a mutation/DDL target (`42809` — a catalog relation is
/// read-only, introspection.md §5); the privilege gate uses it so a built-in is SELECT-gated
/// exactly like a user table under an explicit-grant session envelope.
pub(crate) fn is_catalog_rel_name(name: &str) -> bool {
    catalog_rel_kind(name).is_some()
}

/// The access-method name rendered by `jed_indexes.method` (introspection.md §5.1): the PostgreSQL
/// `amname` spelling of the index kind.
pub(crate) fn index_method_name(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Btree => "btree",
        IndexKind::Gin => "gin",
        IndexKind::Gist => "gist",
    }
}

/// The operator symbol rendered by `jed_constraints.operators` (introspection.md §5.1): the `WITH`
/// operator of an `EXCLUDE` element as written.
pub(crate) fn exclusion_op_symbol(op: ExclusionOp) -> &'static str {
    match op {
        ExclusionOp::Overlaps => "&&",
        ExclusionOp::Equal => "=",
    }
}

/// Reject a mutation target (INSERT / UPDATE / DELETE / CREATE INDEX ON) naming a built-in catalog
/// relation: `42809` wrong_object_type, `cannot modify system relation` (introspection.md §5 — the
/// relations are read-only computed views of the catalog). Checked by NAME, before qualifier
/// validation: the built-in resolves in every database's namespace, so the rejection is
/// scope-independent.
pub(crate) fn check_catalog_rel_write(name: &str) -> Result<()> {
    if is_catalog_rel_name(name) {
        return Err(EngineError::new(
            SqlState::WrongObjectType,
            format!("cannot modify system relation \"{}\"", name),
        ));
    }
    Ok(())
}

/// Build the FIXED synthetic schema of a catalog relation (introspection.md §5). Unlike an SRF's
/// single-column alias rule, a FROM alias renames the RELATION only — the column names are part of
/// the introspection surface. Growth is by ADDING columns (consumers select by name, not position
/// — §5).
pub(crate) fn catalog_rel_table(kind: SrfKind) -> Box<Table> {
    let mk_col = |name: &str, ty: Type, not_null: bool| Column {
        name: name.to_string(),
        ty,
        decimal: None,
        varchar_len: None,
        temporal: None,
        primary_key: false,
        not_null,
        default: None,
        default_expr: None,
        identity: None,
        collation: None,
        no_collation: false,
        collation_conflict: false,
    };
    let col = |name: &str, ty: ScalarType, not_null: bool| mk_col(name, Type::Scalar(ty), not_null);
    // A `text[]` member-list column (introspection.md §5.1 — the indexed / member column names).
    let text_arr = |name: &str, not_null: bool| {
        mk_col(
            name,
            Type::Array(Box::new(Type::Scalar(ScalarType::Text))),
            not_null,
        )
    };
    // A `boolean[]` per-key-element flag column (jed_indexes.descending / nulls_first, §5.1).
    let bool_arr = |name: &str, not_null: bool| {
        mk_col(
            name,
            Type::Array(Box::new(Type::Scalar(ScalarType::Bool))),
            not_null,
        )
    };
    let table = |name: &str, columns: Vec<Column>| {
        Box::new(Table {
            name: name.to_string(),
            columns,
            pk: Vec::new(),
            checks: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            exclusions: Vec::new(),
            on_commit: crate::catalog::OnCommit::PreserveRows,
            scalar_func: false,
        })
    };
    match kind {
        SrfKind::JedTables => table("jed_tables", vec![col("name", ScalarType::Text, true)]),
        SrfKind::JedColumns => table(
            "jed_columns",
            vec![
                col("table_name", ScalarType::Text, true),
                col("name", ScalarType::Text, true),
                col("ordinal", ScalarType::Int32, true),
                col("type", ScalarType::Text, true),
                col("not_null", ScalarType::Bool, true),
                col("pk_ordinal", ScalarType::Int32, false),
                // The DEFAULT as canonical SQL text; NULL for none (introspection.md §5).
                col("column_default", ScalarType::Text, false),
                // 'ALWAYS' / 'BY DEFAULT' for an identity column, else NULL (introspection.md §5).
                col("identity_generation", ScalarType::Text, false),
            ],
        ),
        SrfKind::JedIndexes => table(
            "jed_indexes",
            vec![
                col("name", ScalarType::Text, true),
                col("table_name", ScalarType::Text, true),
                text_arr("columns", true),
                col("is_unique", ScalarType::Bool, true),
                col("method", ScalarType::Text, true),
                // A partial index's predicate canonical text; NULL for a non-partial index
                // (indexes.md §9, introspection.md §5.1).
                col("predicate", ScalarType::Text, false),
                // Per key element, parallel to `columns`: whether it is DESC, and whether its NULLs
                // sort first (indexes.md §11 — PostgreSQL's pg_index.indoption bits).
                bool_arr("descending", true),
                bool_arr("nulls_first", true),
            ],
        ),
        SrfKind::JedConstraints => table(
            "jed_constraints",
            vec![
                col("name", ScalarType::Text, true),
                col("table_name", ScalarType::Text, true),
                col("type", ScalarType::Text, true),
                text_arr("columns", false),
                col("expression", ScalarType::Text, false),
                col("ref_table", ScalarType::Text, false),
                text_arr("ref_columns", false),
                // EXCLUDE: the WITH operators in element order; NULL otherwise (§5.1).
                text_arr("operators", false),
                // FOREIGN KEY: the match type, 'simple' | 'full'; NULL otherwise (§5.1).
                col("match_type", ScalarType::Text, false),
            ],
        ),
        SrfKind::JedStatistics => table(
            "jed_statistics",
            vec![
                col("table_name", ScalarType::Text, true),
                col("column_name", ScalarType::Text, true),
                col("analyzed_rows", ScalarType::Int64, true),
                col("is_stale", ScalarType::Bool, true),
                col("null_count", ScalarType::Int64, true),
                col("distinct_count", ScalarType::Int64, false),
                col("sample_rows", ScalarType::Int64, true),
                col("average_width", ScalarType::Int64, false),
                col("mcv_count", ScalarType::Int32, true),
                col("histogram_count", ScalarType::Int32, true),
                // GIN term-list length; NULL for a term-ineligible column (statistics.md §8).
                col("term_count", ScalarType::Int32, false),
            ],
        ),
        SrfKind::JedSequences => table(
            "jed_sequences",
            vec![
                col("name", ScalarType::Text, true),
                col("start_value", ScalarType::Int64, true),
                col("min_value", ScalarType::Int64, true),
                col("max_value", ScalarType::Int64, true),
                col("increment_by", ScalarType::Int64, true),
                col("cycle", ScalarType::Bool, true),
                col("cache_size", ScalarType::Int64, true),
                // NULL until the sequence has been read (PG's pg_sequences rule —
                // introspection.md §5.3).
                col("last_value", ScalarType::Int64, false),
                // The OWNED BY link of a serial / IDENTITY column; NULL for a standalone sequence.
                col("owned_by_table", ScalarType::Text, false),
                col("owned_by_column", ScalarType::Text, false),
            ],
        ),
        SrfKind::JedTypes => table(
            "jed_types",
            vec![
                col("name", ScalarType::Text, true),
                col("kind", ScalarType::Text, true),
            ],
        ),
        SrfKind::JedTypeFields => table(
            "jed_type_fields",
            vec![
                col("type_name", ScalarType::Text, true),
                col("name", ScalarType::Text, true),
                col("ordinal", ScalarType::Int32, true),
                col("type", ScalarType::Text, true),
                col("not_null", ScalarType::Bool, true),
            ],
        ),
        SrfKind::JedViews => table(
            "jed_views",
            vec![
                col("name", ScalarType::Text, true),
                col("definition", ScalarType::Text, true),
                text_arr("columns", true),
                col("check_option", ScalarType::Text, true),
                col("security_barrier", ScalarType::Bool, true),
                col("security_invoker", ScalarType::Bool, true),
            ],
        ),
        _ => unreachable!("only catalog-relation kinds reach catalog_rel_table"),
    }
}

/// Map a parsed referential action to its persisted form (constraints.md §6.6).
pub(crate) fn fk_action(a: RefAction, _clause: &str) -> Result<FkAction> {
    Ok(match a {
        RefAction::NoAction => FkAction::NoAction,
        RefAction::Restrict => FkAction::Restrict,
        RefAction::Cascade => FkAction::Cascade,
        RefAction::SetNull => FkAction::SetNull,
        RefAction::SetDefault => FkAction::SetDefault,
    })
}

/// A column-ordinal list as a sorted, deduplicated set (for the order-independent FK
/// referenced-columns ⇄ PK/unique-key set comparison — spec/design/constraints.md §6.2).
pub(crate) fn sorted_unique(v: &[usize]) -> Vec<usize> {
    let mut s = v.to_vec();
    s.sort_unstable();
    s.dedup();
    s
}

/// A set-returning function's SYNTHETIC one-column relation (spec/design/functions.md §10). The
/// table's name is the function name (the un-aliased label fallback); the lone column's name is
/// [`srf_column_name`]'s and its type is `col_ty`. The relation is marked as a scalar function's, so
/// its label also works as a whole-row reference to its column ([`Scope::resolve_bare`]).
pub(crate) fn srf_table(func_name: &str, alias: Option<&str>, col_ty: Type) -> Box<Table> {
    Box::new(Table {
        name: func_name.to_string(),
        columns: vec![Column {
            name: srf_column_name(func_name, alias),
            ty: col_ty,
            decimal: None,
            varchar_len: None,
            temporal: None,
            primary_key: false,
            not_null: false,
            default: None,
            default_expr: None,
            identity: None,
            collation: None,
            no_collation: false,
            collation_conflict: false,
        }],
        pk: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        exclusions: Vec::new(),
        on_commit: crate::catalog::OnCommit::PreserveRows,
        scalar_func: true,
    })
}

/// A single-column SRF's output column name — PostgreSQL's `chooseScalarFunctionAlias`, driven by
/// the catalog's `column`/`column_source` (functions.md §10): a function with a named OUT parameter
/// (`jsonb_array_elements` → `value`) is named by it even under a table alias; any other is named
/// by the table alias when one is given, else by the function name. A column-alias list renames
/// the result afterwards.
pub(crate) fn srf_column_name(func_name: &str, alias: Option<&str>) -> String {
    if let Some(d) = SET_RETURNING.iter().find(|d| d.name == func_name)
        && d.column_source == "out_param"
    {
        return d.column.to_string();
    }
    alias.unwrap_or(func_name).to_string()
}

/// Apply a table function's optional rename-only column-alias list (grammar.md §35). Names replace
/// fixed synthetic output columns left-to-right; a shorter list is a partial rename, while a longer
/// list is PostgreSQL's 42P10 invalid-column-reference error.
pub(crate) fn apply_srf_column_aliases(
    table: &mut Table,
    relation_name: &str,
    aliases: Option<&[String]>,
) -> Result<()> {
    let Some(aliases) = aliases else {
        return Ok(());
    };
    if aliases.len() > table.columns.len() {
        return Err(EngineError::new(
            SqlState::InvalidColumnReference,
            format!(
                "table \"{}\" has {} columns available but {} columns specified",
                relation_name,
                table.columns.len(),
                aliases.len()
            ),
        ));
    }
    for (column, alias) in table.columns.iter_mut().zip(aliases) {
        column.name = alias.clone();
    }
    Ok(())
}

/// The catalog name of a json two-column SRF, for its non-object error message.
pub(crate) fn srf_kind_name(kind: SrfKind) -> &'static str {
    match kind {
        SrfKind::JsonbEach => "jsonb_each",
        SrfKind::JsonbEachText => "jsonb_each_text",
        _ => unreachable!("srf_kind_name is only for the json two-column SRFs"),
    }
}

/// A MULTI-COLUMN synthetic table for a set-returning function (C0, json-table.md §1) — the
/// generalization of [`srf_table`] to N named/typed columns. The column NAMES are fixed by the
/// function (e.g. `jsonb_each` → `key`, `value`); the FROM alias renames the relation, not its
/// columns. Used by `json[b]_each[_text]` (and, with a col-def list, the record functions).
pub(crate) fn srf_table_cols(
    func_name: &str,
    alias: Option<&str>,
    cols: Vec<(&str, Type)>,
) -> Box<Table> {
    Box::new(Table {
        name: alias.unwrap_or(func_name).to_string(),
        columns: cols
            .into_iter()
            .map(|(name, ty)| Column {
                name: name.to_string(),
                ty,
                decimal: None,
                varchar_len: None,
                temporal: None,
                primary_key: false,
                not_null: false,
                default: None,
                default_expr: None,
                identity: None,
                collation: None,
                no_collation: false,
                collation_conflict: false,
            })
            .collect(),
        pk: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        exclusions: Vec::new(),
        on_commit: crate::catalog::OnCommit::PreserveRows,
        scalar_func: false,
    })
}
