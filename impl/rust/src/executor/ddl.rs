//! Statement dispatch and DDL execution (mirrors impl/go ddl.go): the post-privilege traffic cop
//! (dispatch_stmt/dispatch_stmt_body) and the DDL executors — CREATE/DROP TABLE (with CHECK/DEFAULT/
//! serial resolution), CREATE/DROP INDEX, CREATE/DROP TYPE, CREATE/DROP/ALTER SEQUENCE — as Engine methods.

use super::*;

/// Walk a resolved index key / predicate expression and collect the **host-function dependencies**
/// it references (spec/design/extensibility.md §8.1, delivery step 4), enforcing the two admission
/// rules a host function must meet to back a **persisted** index:
///   * `IMMUTABLE` — PostgreSQL requires index-expression functions to be immutable; jed adds it
///     because a `volatile`/`stable` function would make the stored keys meaningless (`42P17`).
///   * a `component_id` — without a stable identity + version the file cannot record a dependency it
///     can re-check on reopen (`42P17`, §7). (This is what fixes the latent bug where any host
///     function — even a volatile one — silently leaked into an index expression.)
/// Deduplicated by `(name, arg_types)`; the caller sorts for a stable on-disk order.
fn collect_index_host_deps(
    node: &RExpr,
    reg: &crate::extension::ExtensionRegistry,
    out: &mut Vec<HostFuncDep>,
) -> Result<()> {
    collect_host_deps(node, reg, true, "an index expression", out)
}

/// Collect a resolved `CHECK` expression's host-function dependencies
/// (spec/design/extensibility.md §7.1.2), sorted for the on-disk order. Unlike an index, a check has
/// no volatility gate — the built-in rule admits `CHECK (ts < now())`, and nothing relies on a check
/// being immutable — but each host function must declare a component identity (`42P17`) so a swapped
/// implementation is detectable. Empty for a check that calls no host function.
pub(crate) fn collect_check_host_deps(
    node: &RExpr,
    reg: &crate::extension::ExtensionRegistry,
) -> Result<Vec<HostFuncDep>> {
    let mut deps = Vec::new();
    collect_host_deps(node, reg, false, "a check constraint", &mut deps)?;
    sort_host_deps(&mut deps);
    Ok(deps)
}

/// A stable on-disk / cross-core order (CLAUDE.md §8): sort dependencies by (name, on-disk arg-type
/// codes) so registration order never leaks into the file bytes.
fn sort_host_deps(deps: &mut [HostFuncDep]) {
    let dep_sort_key = |d: &HostFuncDep| {
        (
            d.name.clone(),
            d.arg_types
                .iter()
                .map(|t| crate::format::type_code_for_scalar(*t))
                .collect::<Vec<u8>>(),
        )
    };
    deps.sort_by_key(dep_sort_key);
}

/// The shared walk behind [`collect_index_host_deps`] / [`collect_check_host_deps`]: it admits each
/// host call (`require_immutable` only for an index) and appends one dependency per distinct
/// `(name, arg types)` signature to `out`.
fn collect_host_deps(
    node: &RExpr,
    reg: &crate::extension::ExtensionRegistry,
    require_immutable: bool,
    site: &str,
    out: &mut Vec<HostFuncDep>,
) -> Result<()> {
    if let RExpr::HostFunc { id, name, .. } = node {
        let hf = reg.function(*id);
        if require_immutable && hf.volatility != crate::extension::Volatility::Immutable {
            return Err(EngineError::new(
                SqlState::InvalidObjectDefinition,
                format!("host function {name} in {site} must be IMMUTABLE"),
            ));
        }
        let Some(component_id) = hf.component_id.clone() else {
            return Err(EngineError::new(
                SqlState::InvalidObjectDefinition,
                format!("host function {name} used in {site} must declare a component identity"),
            ));
        };
        if !out
            .iter()
            .any(|d| d.name == hf.name && d.arg_types == hf.arg_types)
        {
            out.push(HostFuncDep {
                name: hf.name.clone(),
                arg_types: hf.arg_types.clone(),
                result: hf.result,
                component_id,
                semantic_version: hf.semantic_version,
            });
        }
    }
    for child in estimator_expression_children(node) {
        collect_host_deps(child, reg, require_immutable, site, out)?;
    }
    Ok(())
}

/// Verify a resolved index key / predicate expression's host-function references still match the
/// dependencies persisted with the index (spec/design/extensibility.md §8.1/§11, delivery step 4) —
/// the reopen soundness check. Called on every per-statement re-resolution: if the current registry
/// supplies a *different* `component_id` or a *bumped* `semantic_version` for a signature the index
/// depends on, the stored keys are stale, so this returns `XX002`, which the **read** path swallows
/// (`resolve_index(...).ok()?` → the planner skips the index, a correct heap scan) and a **write**
/// propagates (the table is read-only until a rebuild). A *missing* function is caught earlier by
/// resolution itself (`42883`). A no-op for an index that records no host dependency (the common case,
/// and for GIN/GiST whose keys are plain columns).
fn verify_index_host_deps(
    node: &RExpr,
    reg: &crate::extension::ExtensionRegistry,
    deps: &[HostFuncDep],
) -> Result<()> {
    if let RExpr::HostFunc { id, name, .. } = node {
        if !host_dep_matches(reg.function(*id), deps) {
            return Err(EngineError::new(
                // XX002 — the graded "a versioned dependency no longer matches" reopen verdict, shared
                // with the collation version-skew case (extensibility.md §11/§15).
                SqlState::CollationVersionMismatch,
                format!(
                    "index depends on host function {name} whose registered component/version no \
                     longer matches the file; the index is unusable until it is rebuilt"
                ),
            ));
        }
    }
    for child in estimator_expression_children(node) {
        verify_index_host_deps(child, reg, deps)?;
    }
    Ok(())
}

/// [`verify_index_host_deps`] for a `CHECK` (spec/design/extensibility.md §7.1.2): every host call
/// the resolved check makes must match a dependency persisted with it, else `XX002`. Unlike an index
/// there is no read-side fallback to take — only a statement that evaluates the check resolves it. A
/// check that persisted no dependency but now resolves to a host function fails the same way.
fn verify_check_host_deps(
    check_name: &str,
    node: &RExpr,
    reg: &crate::extension::ExtensionRegistry,
    deps: &[HostFuncDep],
) -> Result<()> {
    if let RExpr::HostFunc { id, name, .. } = node {
        if !host_dep_matches(reg.function(*id), deps) {
            return Err(EngineError::new(
                SqlState::CollationVersionMismatch,
                format!(
                    "check constraint {check_name} depends on host function {name} whose registered \
                     component/version no longer matches the file; the constraint must be re-added"
                ),
            ));
        }
    }
    for child in estimator_expression_children(node) {
        verify_check_host_deps(check_name, child, reg, deps)?;
    }
    Ok(())
}

/// Whether a resolved host function's current registration matches one of `deps` by signature,
/// component identity, and semantic version.
fn host_dep_matches(hf: &crate::extension::HostFunction, deps: &[HostFuncDep]) -> bool {
    deps.iter().any(|d| {
        d.name == hf.name
            && d.arg_types == hf.arg_types
            && hf.component_id.as_deref() == Some(d.component_id.as_str())
            && hf.semantic_version == d.semantic_version
    })
}

/// A foreign key never crosses between the session-local temp domain and the main database
/// (spec/design/temp-tables.md §8): PostgreSQL's ATAddForeignKeyConstraint persistence rule, 42P16,
/// checked once the parent resolves.
fn check_fk_persistence(child_temp: bool, parent_temp: bool) -> Result<()> {
    if child_temp && !parent_temp {
        return Err(EngineError::new(
            SqlState::InvalidTableDefinition,
            "constraints on temporary tables may reference only temporary tables",
        ));
    }
    if !child_temp && parent_temp {
        return Err(EngineError::new(
            SqlState::InvalidTableDefinition,
            "constraints on permanent tables may reference only permanent tables",
        ));
    }
    Ok(())
}

/// The error for a FOREIGN KEY parent that is not a table of the parent's database: a view is the
/// wrong object kind (42809, views.md §2); any other name is missing (42P01).
fn fk_parent_missing(snap: &Snapshot, name: &str) -> EngineError {
    if snap.view(name).is_some() {
        return EngineError::new(
            SqlState::WrongObjectType,
            format!("referenced relation \"{name}\" is not a table"),
        );
    }
    EngineError::new(
        SqlState::UndefinedTable,
        format!("table does not exist: {name}"),
    )
}

/// PostgreSQL's `ConstraintNameExists` for a derived name's collision walk (constraints.md §6.7):
/// whether a table of the database domain `snap` — the main database, the session's temp domain, or
/// one attachment, each its own schema — holds a constraint named `name`. `skip` is the table being
/// created or altered, whose in-progress definition the caller checks itself; every other table is
/// read as the statement found it.
fn schema_constraint_name_taken(snap: &Snapshot, skip: &str, name: &str) -> bool {
    snap.tables
        .iter()
        .any(|(key, t)| key != skip && constraint_name_taken(t, name))
}

fn constraint_name_taken(t: &Table, name: &str) -> bool {
    t.checks.iter().any(|c| c.name == name)
        || t.indexes.iter().any(|i| i.unique && i.name == name)
        || t.foreign_keys.iter().any(|f| f.name == name)
        || t.exclusions.iter().any(|e| e.name == name)
}

/// Resolve the parts of an EXCLUDE definition that are independent of naming. CREATE TABLE and
/// ALTER TABLE must share this path so their operator/opclass and duplicate-column gates cannot
/// drift (spec/design/gist.md §7).
fn resolve_exclusion_definition(
    table: &Table,
    using: Option<&str>,
    elements: &[(String, String)],
) -> Result<(Vec<usize>, Vec<ExclusionElement>)> {
    if let Some(method) = using {
        if method != "gist" {
            return Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("access method {method} does not support exclusion constraints"),
            ));
        }
    }
    let mut columns = Vec::with_capacity(elements.len());
    let mut resolved = Vec::with_capacity(elements.len());
    for (column_name, operator) in elements {
        let column = table
            .columns
            .iter()
            .position(|c| c.name == *column_name)
            .ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedColumn,
                    format!("column {column_name} named in key does not exist"),
                )
            })?;
        if columns.contains(&column) {
            return Err(EngineError::new(
                SqlState::DuplicateColumn,
                format!("column {column_name} appears twice in exclusion constraint"),
            ));
        }
        let ty = &table.columns[column].ty;
        let op = match operator.as_str() {
            "&&" => {
                if ty.range_element().is_none() {
                    return Err(EngineError::new(
                        SqlState::UndefinedObject,
                        format!(
                            "data type {} has no default operator class for access method gist that accepts operator &&",
                            ty.canonical_name()
                        ),
                    ));
                }
                ExclusionOp::Overlaps
            }
            "=" => {
                if is_gist_scalar_type(ty) {
                    ExclusionOp::Equal
                } else if is_gist_deferred_scalar_type(ty) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "an exclusion constraint with = over {} is not supported yet",
                            ty.canonical_name()
                        ),
                    ));
                } else {
                    return Err(EngineError::new(
                        SqlState::UndefinedObject,
                        format!(
                            "data type {} has no default operator class for access method gist",
                            ty.canonical_name()
                        ),
                    ));
                }
            }
            other => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!("exclusion constraint operator {other} is not supported yet"),
                ));
            }
        };
        columns.push(column);
        resolved.push(ExclusionElement { column, op });
    }
    Ok((columns, resolved))
}

/// Where an identity column's owned sequence lives during an `ALTER TABLE` action list
/// (alter.md §2.8): pending (created by an earlier ADD COLUMN / ADD IDENTITY action of the same
/// statement) or already in the catalog under `key`.
enum IdentitySeqSlot {
    Pending(usize),
    Existing(String),
}

/// Locate identity column `ci`'s owned sequence. The column's synthesized `nextval('<seq>')`
/// default names it, which tells the identity sequence apart from a leftover `serial` sequence the
/// same column may still own after `DROP DEFAULT`. `old_ci` is the column's pre-statement ordinal
/// (`None` for a column added by this statement).
fn identity_sequence_slot(
    snap: &Snapshot,
    original_name: &str,
    table: &Table,
    ci: usize,
    old_ci: Option<usize>,
    pending: &[SequenceDef],
    dropped: &std::collections::HashSet<String>,
) -> IdentitySeqSlot {
    // The default is `nextval ( '<seq>' )`, possibly wrapped in the CAST an earlier
    // `ALTER COLUMN … TYPE` added; the quoted name inside the call identifies the sequence.
    let default_text = table.columns[ci]
        .default_expr
        .as_ref()
        .map_or("", |d| d.expr_text.as_str());
    let names_it = |name: &str| default_text.contains(nextval_default_text(name).as_str());
    let pending_owned = |seq: &SequenceDef| {
        seq.owned_by
            .as_ref()
            .is_some_and(|o| o.table == table.name && usize::from(o.column) == ci)
    };
    if let Some(si) = pending
        .iter()
        .position(|seq| pending_owned(seq) && names_it(&seq.name))
        .or_else(|| pending.iter().position(pending_owned))
    {
        return IdentitySeqSlot::Pending(si);
    }
    let old_ci = old_ci.expect("an added identity column has a pending sequence");
    let owned: Vec<String> = snap
        .sequences_owned_by(original_name)
        .into_iter()
        .filter(|key| {
            !dropped.contains(key)
                && snap.sequence(key).is_some_and(|seq| {
                    seq.owned_by
                        .as_ref()
                        .is_some_and(|o| usize::from(o.column) == old_ci)
                })
        })
        .collect();
    let key = owned
        .iter()
        .find(|key| names_it(key))
        .or(owned.first())
        .cloned()
        .expect("identity column has an owned sequence");
    IdentitySeqSlot::Existing(key)
}

/// 55000 for an identity alteration of a column that is not an identity column (PostgreSQL's
/// `ATExecSetIdentity` / `ATExecDropIdentity`).
fn not_identity_column(table: &Table, ci: usize) -> EngineError {
    EngineError::new(
        SqlState::ObjectNotInPrerequisiteState,
        format!(
            "column \"{}\" of relation \"{}\" is not an identity column",
            table.columns[ci].name, table.name
        ),
    )
}

/// Retype an IDENTITY-owned sequence with PostgreSQL's `ALTER SEQUENCE AS` rules: bounds equal to
/// the old type defaults become the new defaults, explicit bounds are preserved, and the start +
/// current value must fit the new type and resulting bounds.
fn retype_identity_sequence(
    existing: &SequenceDef,
    old_type: SeqDataType,
    new_type: SeqDataType,
) -> Result<SequenceDef> {
    let mut def = existing.clone();
    let (old_min, old_max) = old_type.default_bounds(def.increment);
    let (new_min, new_max) = new_type.default_bounds(def.increment);
    if def.min_value == old_min {
        def.min_value = new_min;
    }
    if def.max_value == old_max {
        def.max_value = new_max;
    }
    let (type_min, type_max) = new_type.range();
    if def.max_value > type_max {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!(
                "MAXVALUE ({}) is out of range for sequence data type {}",
                def.max_value,
                new_type.pg_name()
            ),
        ));
    }
    if def.min_value < type_min {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!(
                "MINVALUE ({}) is out of range for sequence data type {}",
                def.min_value,
                new_type.pg_name()
            ),
        ));
    }
    if def.min_value >= def.max_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!(
                "MINVALUE ({}) must be less than MAXVALUE ({})",
                def.min_value, def.max_value
            ),
        ));
    }
    seq_bound_check_start(def.start, def.min_value, def.max_value)?;
    seq_bound_check_last(def.last_value, def.min_value, def.max_value)?;
    Ok(def)
}

impl Engine {
    /// Whether a resolved column [`Type`] is **key-encodable** — usable as a `PRIMARY KEY` / ordered
    /// secondary index / `UNIQUE` member (spec/design/encoding.md §2). The keyable scalars
    /// ([`is_keyable_scalar`]), the `range` container, a scalar-element `array`
    /// ([`is_array_keyable`]), or a **composite** whose fields are ALL keyable (recursive, §2.15 —
    /// resolved against the current snapshot's type catalog; an `array`-of-composite field makes it
    /// unkeyable). The single gate every PK / UNIQUE / CREATE INDEX site calls.
    pub(crate) fn type_is_keyable(&self, ty: &Type) -> bool {
        is_keyable_scalar(ty)
            || ty.is_range()
            || is_array_keyable(ty)
            || (ty.is_composite() && self.read_snap().resolve_col_type(ty).keyable())
    }

    /// The `CREATE INDEX` column-type gate for one index kind (indexes.md §1, gin.md §3, gist.md §6):
    /// an ordered index needs a key-encodable type (`0A000`), GIN and GiST an operator class for the
    /// type (`42704` for none, `0A000` for a deferred one). `ALTER COLUMN … TYPE` re-applies it to
    /// every surviving index on a retyped column (alter.md §3.3).
    pub(crate) fn check_index_column_type(&self, kind: IndexKind, ty: &Type) -> Result<()> {
        match kind {
            IndexKind::Btree => {
                if !self.type_is_keyable(ty) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "a {} index column is not supported yet",
                            ty.canonical_name()
                        ),
                    ));
                }
            }
            IndexKind::Gin => {
                // GIN needs an operator class for the column type (gin.md §3): `jsonb_ops` for a
                // jsonb column (§2.1); `array_ops` for an array whose element is an admitted
                // key-encodable type (is_gin_element_type — the integers, boolean, uuid, date,
                // timestamp, timestamptz, f32, f64, interval, text, bytea, decimal), else 0A000. Any
                // other type has no GIN operator class (42704).
                if ty.is_jsonb() {
                    return Ok(());
                }
                match ty.array_element() {
                    None => {
                        return Err(EngineError::new(
                            SqlState::UndefinedObject,
                            format!(
                                "data type {} has no default operator class for access method gin",
                                ty.canonical_name()
                            ),
                        ));
                    }
                    Some(elem) if !is_gin_element_type(&elem) => {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            format!(
                                "a gin index on {} is not supported yet",
                                ty.canonical_name()
                            ),
                        ));
                    }
                    Some(_) => {}
                }
            }
            IndexKind::Gist => {
                // GiST opclasses (spec/design/gist.md §5/§6): `range_ops` over a range column,
                // or the in-core `btree_gist`-equivalent scalar `=` opclass over a FIXED-WIDTH
                // keyable scalar (integers / boolean / uuid / date / timestamp / timestamptz —
                // its bound is `[min, max]` over that type's order-preserving key encoding, all
                // pure byte comparison). A keyable-but-deferred scalar (text / bytea / decimal /
                // interval) is 0A000 — we will support it (the GIN element-staging precedent,
                // §11); any other type (float / json / array / composite / jsonpath) has no GiST
                // opclass at all — 42704 (PG's wording, the GIN-no-opclass precedent).
                if !ty.is_range() {
                    if is_gist_scalar_type(ty) {
                        // supported scalar `=` opclass — ok
                    } else if is_gist_deferred_scalar_type(ty) {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            format!(
                                "a gist index on {} is not supported yet",
                                ty.canonical_name()
                            ),
                        ));
                    } else {
                        return Err(EngineError::new(
                            SqlState::UndefinedObject,
                            format!(
                                "data type {} has no default operator class for access method gist",
                                ty.canonical_name()
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Dispatch one parsed statement to its executor. The autocommit transaction handling
    /// (capture / durable commit / rollback-on-error) lives in `execute_stmt_params`.
    pub(crate) fn dispatch_stmt(&mut self, stmt: Statement, params: &[Value]) -> Result<Outcome> {
        self.dispatch_stmt_cached(stmt, params, None, false)
    }

    /// `dispatch_stmt` with the private prepared-INSERT slot. Admission stays before cache lookup,
    /// so a hit cannot bypass lifetime or privilege gates (spec/design/api.md §2.4).
    pub(crate) fn dispatch_stmt_cached(
        &mut self,
        stmt: Statement,
        params: &[Value],
        insert_cache: Option<&std::cell::RefCell<Option<CachedInsert>>>,
        allow_insert_fill: bool,
    ) -> Result<Outcome> {
        // Lifetime budget admission (spec/design/session.md §5.4): once the session's cumulative cost
        // has reached `lifetime_max_cost`, every further statement is rejected `54P02` **before it can
        // accrue** — checked ahead of privileges/existence, so an exhausted session runs nothing. A
        // no-op when the budget is unlimited (the default). Transaction control (BEGIN/COMMIT/ROLLBACK)
        // never reaches dispatch (handled in `execute_stmt_params`), so an exhausted session can still
        // close out an open block.
        self.check_lifetime_admission()?;
        // Authorization (spec/design/session.md §5.3): enforce the session's privilege envelope
        // before the statement runs — DDL gated by `allow_ddl`, DML by per-table/per-function
        // privileges, all `42501`. Skipped on a fully-permissive session (the default), so the
        // common path pays nothing. The physical access-mode gate (`25006`) is checked earlier in
        // `execute_stmt_params`, so it wins when both apply.
        self.check_privileges(&stmt)?;
        let out = self.dispatch_stmt_body_cached(stmt, params, insert_cache, allow_insert_fill);
        // Keep each GiST index's resident R-tree current: after a statement that mutated the main
        // image, rebuild it from the (now-updated) leaf store so the next read descends a fresh tree
        // (spec/design/gist.md §3/§4.1). A no-op for reads / temp-only writes (main_dirty unset).
        if out.is_ok() {
            self.rebuild_main_gist_trees_if_dirty()?;
        }
        out
    }

    /// Route one parsed statement to its executor (the equivalent-serial `dispatchStmtBody`): the raw
    /// dispatch WITHOUT the lifetime/privilege admission and the GiST rebuild that wrap it in
    /// [`Self::dispatch_stmt`]. Split out so `EXPLAIN ANALYZE` can execute its inner statement's body
    /// directly — the admission + rebuild already ran on the enclosing EXPLAIN (spec/design/explain.md §3).
    pub(crate) fn dispatch_stmt_body(
        &mut self,
        stmt: Statement,
        params: &[Value],
    ) -> Result<Outcome> {
        self.dispatch_stmt_body_cached(stmt, params, None, false)
    }

    pub(crate) fn dispatch_stmt_body_cached(
        &mut self,
        stmt: Statement,
        params: &[Value],
        insert_cache: Option<&std::cell::RefCell<Option<CachedInsert>>>,
        allow_insert_fill: bool,
    ) -> Result<Outcome> {
        // The database a table-naming error concerns (spec/design/error-fields.md §12): the
        // statement's target qualifier, else the implicit scope. UPDATE and DELETE stamp their own
        // (they also run as writable-CTE children).
        let target_db = match &stmt {
            Statement::CreateTable(ct) if ct.temp => Some("temp".to_string()),
            Statement::CreateTable(ct) => ct.db.clone(),
            Statement::AlterTable(at) => at.db.clone(),
            Statement::CreateIndex(ci) => ci.db.clone(),
            Statement::Insert(ins) => ins.db.clone(),
            _ => None,
        };
        self.dispatch_stmt_target(stmt, params, insert_cache, allow_insert_fill)
            .map_err(|e| self.stamp_database(target_db.as_deref(), e))
    }

    fn dispatch_stmt_target(
        &mut self,
        stmt: Statement,
        params: &[Value],
        insert_cache: Option<&std::cell::RefCell<Option<CachedInsert>>>,
        allow_insert_fill: bool,
    ) -> Result<Outcome> {
        match stmt {
            Statement::Analyze(analyze) => {
                reject_params_for_ddl(params)?;
                self.execute_analyze(analyze)
            }
            // `CREATE TABLE … AS` runs a query, so its `$N` parameters bind like the query's
            // (spec/design/create-table-as.md §1); the column-definition form takes none.
            Statement::CreateTable(ct) if ct.as_query.is_some() => {
                self.execute_create_table_as(ct, params)
            }
            Statement::CreateTable(ct) => {
                reject_params_for_ddl(params)?;
                self.execute_create_table(ct)
            }
            Statement::DropTable(dt) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_table(dt)
            }
            Statement::AlterTable(at) => {
                reject_params_for_ddl(params)?;
                self.execute_alter_table(at)
            }
            Statement::CreateIndex(ci) => {
                reject_params_for_ddl(params)?;
                self.execute_create_index(ci)
            }
            Statement::DropIndex(di) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_index(di)
            }
            Statement::Reindex(rx) => {
                reject_params_for_ddl(params)?;
                self.execute_reindex(rx)
            }
            Statement::CreateType(ct) => {
                reject_params_for_ddl(params)?;
                self.execute_create_type(ct)
            }
            Statement::DropType(dt) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_type(dt)
            }
            Statement::CreateSequence(cs) => {
                reject_params_for_ddl(params)?;
                self.execute_create_sequence(cs)
            }
            Statement::DropSequence(ds) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_sequence(ds)
            }
            Statement::AlterSequence(als) => {
                reject_params_for_ddl(params)?;
                self.execute_alter_sequence(als)
            }
            Statement::CreateView(cv) => {
                reject_params_for_ddl(params)?;
                self.execute_create_view(cv)
            }
            Statement::DropView(dv) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_view(dv)
            }
            Statement::AlterView(av) => {
                reject_params_for_ddl(params)?;
                self.execute_alter_view(av)
            }
            Statement::Insert(ins) => self.execute_insert_cached(
                ins,
                params,
                CteCtx::empty(),
                insert_cache,
                allow_insert_fill,
            ),
            Statement::Select(sel) => self.execute_select(sel, params),
            Statement::SetOp(so) => self.execute_set_op(so, params),
            Statement::With(wq) => self.execute_with(wq, params),
            Statement::Update(upd) => self.execute_update(upd, params, CteCtx::empty()),
            Statement::Delete(del) => self.execute_delete(del, params, CteCtx::empty()),
            // EXPLAIN renders the inner statement's plan (spec/design/explain.md): plain EXPLAIN is
            // plan-only, EXPLAIN ANALYZE runs the inner and reports its actual cost + row count.
            Statement::Explain {
                analyze,
                verbose,
                costs,
                lane,
                inner,
            } => self.execute_explain(analyze, verbose, costs, lane, *inner, params),
            // Transaction control is handled by `execute_stmt_params` before dispatch.
            Statement::Begin { .. }
            | Statement::Commit
            | Statement::Rollback
            | Statement::Savepoint { .. } => {
                unreachable!("transaction control is handled before dispatch")
            }
        }
    }

    /// Whether CREATE TABLE's target name is already taken. The relation namespace is shared between
    /// tables and indexes (indexes.md §2), so a collision with either kind is the same 42P07 — PG's
    /// "relation" word. For a bare/main/temp target `relation_exists` is temp-aware
    /// (preclude-overlaps — temp-tables.md §3); an attachment target checks its OWN snapshot's
    /// namespace (each attached database is independent, attached-databases.md §3).
    /// The database domain (schema) a CREATE TABLE's new table joins, for the derived constraint
    /// name walk (constraints.md §6.7): the session's temp domain, an attachment, or main.
    fn create_table_domain(&self, target_temp: bool, attach_name: Option<&str>) -> &Snapshot {
        if target_temp {
            return self.temp_read_snap();
        }
        match attach_name {
            Some(attach) => self
                .attach_read_snap(attach)
                .expect("attachment resolved by the caller"),
            None => self.read_snap(),
        }
    }

    pub(crate) fn create_table_name_taken(&self, name: &str, attach_name: Option<&str>) -> bool {
        match attach_name {
            Some(attach) => {
                let as_snap = self
                    .attach_read_snap(attach)
                    .expect("attachment resolved by the caller");
                as_snap.table(name).is_some()
                    || as_snap.find_index(name).is_some()
                    || as_snap.view(name).is_some()
            }
            None => self.relation_exists(name),
        }
    }

    /// Analyze and run a CREATE TABLE: resolve each column's type name, enforce a
    /// single primary key across both forms (column-level and the table-level
    /// `PRIMARY KEY (a, b, …)` constraint — which is implicitly NOT NULL per member),
    /// reject duplicate table and column names, then register the table.
    /// Constraint checks mirror PostgreSQL's order (oracle-probed, constraints.md §3):
    /// a second primary key traps 42P16 before its members resolve; members resolve
    /// left to right (unknown 42703, repeated 42701); then the jed narrowings — the
    /// declaration-order rule and the per-member key-type gate — trap 0A000.
    pub(crate) fn execute_create_table(&mut self, ct: CreateTable) -> Result<Outcome> {
        self.create_table_with_columns(ct, None)
    }

    /// The database a CREATE TABLE writes into (attached-databases.md §3, Slice 1b): whether the new
    /// table is session-local temp, and the attachment it is created in (`None` for main / temp).
    /// `main`/`temp` fold into the implicit scope (main = bare persistent, temp = TEMP); a
    /// host-attached name routes the new table INTO that attachment's working snapshot (§6). TEMP
    /// with an explicit database is contradictory unless the database IS `temp` (42601); an unknown
    /// database is 42P01 and a read-only attachment 25006.
    pub(crate) fn create_table_target(&self, ct: &CreateTable) -> Result<(bool, Option<String>)> {
        let mut target_temp = ct.temp;
        let mut attach_name: Option<String> = None;
        if let Some(qual) = &ct.db {
            match qual.to_string().as_str() {
                "main" => {
                    if ct.temp {
                        return Err(EngineError::new(
                            SqlState::SyntaxError,
                            "cannot create a TEMP table in database \"main\"",
                        ));
                    }
                }
                "temp" => target_temp = true,
                other => {
                    if ct.temp {
                        return Err(EngineError::new(
                            SqlState::SyntaxError,
                            "cannot create a TEMP table in an attached database",
                        ));
                    }
                    let lname = other.to_string();
                    if self.attach_read_snap(&lname).is_none() {
                        return Err(EngineError::new(
                            SqlState::UndefinedTable,
                            format!("database \"{qual}\" is not attached"),
                        ));
                    }
                    // A DDL write to a READ-ONLY attachment is 25006 before any work (§4).
                    self.check_attachment_writable(ct.db.as_deref())?;
                    attach_name = Some(lname);
                }
            }
        }
        Ok((target_temp, attach_name))
    }

    /// CREATE TABLE over either column definitions (`prebuilt` is `None`) or the columns a
    /// `CREATE TABLE … AS` derived from its query (spec/design/create-table-as.md §3), which then
    /// skip the column-definition walk; everything after it — the target checks, the temp and
    /// attachment narrowings, and the registration — is shared.
    pub(crate) fn create_table_with_columns(
        &mut self,
        ct: CreateTable,
        prebuilt: Option<Vec<Column>>,
    ) -> Result<Outcome> {
        // A session-local temporary table (spec/design/temp-tables.md) is built exactly like a
        // persistent one but registered into the session temp snapshot at the end (§2), so it makes
        // zero file writes. The other temp narrowings (composite/collated columns, serial/IDENTITY)
        // are checked just before registration, once the columns are built.
        // Resolve the optional database qualifier (attached-databases.md §3, Slice 1b).
        let (target_temp, attach_name) = self.create_table_target(&ct)?;
        // The reservation is not a collision, so IF NOT EXISTS does not suppress it
        // (spec/design/introspection.md §4).
        check_reserved_name("table", &ct.name)?;
        // IF NOT EXISTS (grammar.md §13): a taken name is a no-op success, decided before any column
        // or constraint work (PostgreSQL's order) — so nothing in the definition is validated and
        // nothing is created (no owned `serial` sequence), and the working snapshot stays untouched.
        // The bare form keeps its 42P07 below, after the narrowings.
        if ct.if_not_exists && self.create_table_name_taken(&ct.name, attach_name.as_deref()) {
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
        }
        // ON COMMIT belongs to temporary tables only — any clause, PRESERVE ROWS included, on a
        // persistent or attached table is 42P16, checked after the IF NOT EXISTS skip and before any
        // column work (PostgreSQL's DefineRelation order; temp-tables.md §5.1).
        if ct.on_commit.is_some() && !target_temp {
            return Err(EngineError::new(
                SqlState::InvalidTableDefinition,
                "ON COMMIT can only be used on temporary tables",
            ));
        }
        if target_temp && !ct.excludes.is_empty() {
            // An EXCLUDE constraint's backing GiST index would live on the temp snapshot — deferred
            // with the rest of the GiST-on-temp narrowing (spec/design/gist.md §11), a clean 0A000
            // before any column resolves.
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "an EXCLUDE constraint on a temporary table is not yet supported",
            ));
        }
        // Deferred narrowings on an attached-database table this slice (attached-databases.md §8), each a
        // clean 0A000 before any column work: FOREIGN KEY and EXCLUDE (their probe/backing structures
        // would need cross-scope catalog access this slice does not thread). Serial/IDENTITY and
        // composite/collated columns are checked just before registration, once the columns are built.
        if attach_name.is_some() {
            if !ct.foreign_keys.is_empty() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "FOREIGN KEY on an attached-database table is not supported yet",
                ));
            }
            if !ct.excludes.is_empty() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "an EXCLUDE constraint on an attached-database table is not supported yet",
                ));
            }
        }
        if self.create_table_name_taken(&ct.name, attach_name.as_deref()) {
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation already exists: {}", ct.name),
            ));
        }

        let mut columns = prebuilt.unwrap_or_else(|| Vec::with_capacity(ct.columns.len()));
        // The primary-key member ordinals in KEY order (constraints.md §3): the column-level
        // form is the one-member case; the table-level list below records its own order.
        let mut pk: Vec<usize> = Vec::new();
        let mut pk_seen = false;
        // The OWNED sequences a `serial` column desugars to (spec/design/sequences.md §12), collected
        // during the column walk and staged into the working snapshot only after the whole CREATE
        // TABLE validates — so a later failure (e.g. a bad CHECK) discards them with the statement.
        let mut pending_serials: Vec<SequenceDef> = Vec::new();
        for def in &ct.columns {
            if columns.iter().any(|c: &Column| c.name == def.name) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("duplicate column name: {}", def.name),
                ));
            }
            // A `serial` / `bigserial` / `smallserial` pseudo-type (spec/design/sequences.md §12):
            // CREATE TABLE sugar for an integer column that is NOT NULL with a DEFAULT nextval(...)
            // backed by a newly-created OWNED sequence. The desugaring (the owned sequence + the
            // default + the NOT NULL force) happens in the default-classification block and the
            // column push below; here we only resolve the underlying integer type. `serial[]` is NOT
            // a serial column (it falls to the array branch as an unknown element type — §12.1).
            let serial_kind = serial_pseudo_type(&def.type_name);
            // Resolve the column type: a built-in scalar, or a user-defined composite referenced by
            // name (spec/design/composite.md §3). An unknown name is 42704. A composite column
            // carries no typmod (the composite's fields carry their own); a type modifier written on
            // a composite column is rejected (0A000). A composite column is storable but never
            // keyable — the PK gate below rejects it 0A000 (§6).
            let mut temporal: Option<TemporalTypmod> = None;
            let (ty, decimal, varchar_len): (Type, Option<DecimalTypmod>, Option<u32>) =
                if let Some(sk) = serial_kind {
                    // A serial column takes no typmod (`serial(5)` is 42601) and no `[]` (handled by
                    // the array branch). Its type is the underlying integer; everything else below.
                    if def.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::SyntaxError,
                            format!("type modifier is not allowed for type {}", def.type_name),
                        ));
                    }
                    (Type::Scalar(sk), None, None)
                } else if let Some(base) = def.type_name.strip_suffix("[]") {
                    // An array column (spec/design/array.md §3). The element type is a scalar or a
                    // previously-defined composite (array-of-composite, §12 AC1 — `element_type_code`
                    // 14 + name); a nested-array element and an array typmod (`numeric(p,s)[]`) stay
                    // deferred (0A000).
                    if def.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "a type modifier on an array type is not supported yet".to_string(),
                        ));
                    }
                    match ScalarType::from_name(base) {
                        Some(s) => (Type::Array(Box::new(Type::Scalar(s))), None, None),
                        None => {
                            if let Some(ctype) = self.read_snap().composite_type(base) {
                                (
                                    Type::Array(Box::new(Type::Composite(
                                        crate::types::CompositeRef {
                                            name: ctype.name.clone(),
                                        },
                                    ))),
                                    None,
                                    None,
                                )
                            } else {
                                return Err(EngineError::new(
                                    SqlState::UndefinedObject,
                                    format!("type does not exist: {base}"),
                                )
                                .with_position(def.type_pos.0));
                            }
                        }
                    }
                } else if let Some(rdesc) = crate::range::range_by_name(&def.type_name) {
                    // A range column (spec/design/ranges.md §3): structural like array, the element
                    // carried inline. A range takes no typmod (`numrange(10,2)` is not a thing — the
                    // element is the unconstrained subtype), so a type modifier is rejected.
                    if def.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "a type modifier on a range type is not supported".to_string(),
                        ));
                    }
                    let elem = crate::range::element_scalar(rdesc);
                    (Type::Range(Box::new(Type::Scalar(elem))), None, None)
                } else if ScalarType::from_name(&def.type_name).is_some() {
                    let (s, tm) = resolve_type_and_typmod(&def.type_name, &def.type_mod)?;
                    // `jsonpath` is literal-only this slice (P1a) — a jsonpath COLUMN is `0A000`, like a
                    // J0-stage json column (a storable jsonpath is a follow-on).
                    if s == ScalarType::JsonPath {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "a jsonpath column is not supported yet".to_string(),
                        ));
                    }
                    temporal = tm.temporal;
                    (Type::Scalar(s), tm.decimal, tm.varchar_len)
                } else if let Some(ctype) = self.read_snap().composite_type(&def.type_name) {
                    if def.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            format!(
                                "a type modifier is not supported for composite type {}",
                                def.type_name
                            ),
                        ));
                    }
                    (
                        Type::Composite(crate::types::CompositeRef {
                            name: ctype.name.clone(),
                        }),
                        None,
                        None,
                    )
                } else {
                    return Err(EngineError::new(
                        SqlState::UndefinedObject,
                        format!("type does not exist: {}", def.type_name),
                    )
                    .with_position(def.type_pos.0));
                };
            if def.primary_key {
                // The key-encodable scalars may be a PRIMARY KEY. The fixed-width ones — integers,
                // boolean (`bool-byte` §2.9), uuid (`uuid-raw16` §2.7), timestamp/timestamptz (i64
                // `int-be-signflip`, spec/design/timestamp.md §6), date (i32, spec/design/date.md §5),
                // interval (`interval-span-i128` — the 16-byte span key, encoding.md §2.10) — plus
                // the variable-width `text`/`bytea` (`…-terminated-escape`, encoding.md §2.4/§2.6) and
                // `decimal` (`decimal-order-preserving` §2.5), all self-delimiting so they compose in
                // composite keys / index suffixes — plus `float` (`float-order-preserving` §2.8) — the
                // `range` container (`range-bounds` §2.11), the `array` container of a keyable scalar
                // element (`array-elements-terminated` §2.14), and the `composite` container of
                // all-keyable fields (`composite-field-slots` §2.15, `type_is_keyable` recursing). Still
                // rejected `0A000`: an array whose element is a composite, and a composite that
                // transitively contains one. An oversized key (one that can't fit a node) trips the
                // existing RECORD_MAX oversized-item 0A000, mirroring PG's btree key-size limit.
                if !self.type_is_keyable(&ty) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!("a {} primary key is not supported yet", ty.canonical_name()),
                    ));
                }
                if pk_seen {
                    return Err(EngineError::new(
                        SqlState::InvalidTableDefinition,
                        format!(
                            "multiple primary keys for table {} are not allowed",
                            ct.name
                        ),
                    ));
                }
                pk_seen = true;
                pk.push(columns.len()); // this column's ordinal (pushed below)
            }
            // Classify the DEFAULT by syntactic form (constraints.md §2). A bad default fails
            // at CREATE TABLE either way; NOT NULL is NOT enforced here (not_null=false), so a
            // `DEFAULT NULL` on a NOT NULL column is accepted and traps 23502 only when applied.
            //   - a bare literal is pre-evaluated + type-coerced to a constant value (the
            //     fast-path: out of range 22003, cross-family 42804, decimal rounded to typmod);
            //   - any other expression is validated (structural pre-walk, then resolved against
            //     an EMPTY scope — a default may not reference a column — then its result type is
            //     checked assignable to the column, 42804) and stored as text for per-row eval.
            // A `serial` pseudo-type OR a `GENERATED … AS IDENTITY` constraint both desugar to an
            // auto-numbered column: an OWNED sequence + a synthesized `DEFAULT nextval(...)` + NOT
            // NULL (sequences.md §12/§13). Identity additionally records ALWAYS/BY DEFAULT and gates
            // the column type to i16/i32/i64.
            let (default, default_expr, identity_kind) = if serial_kind.is_some()
                || def.identity.is_some()
            {
                // IDENTITY type gate: the declared column type must be smallint/integer/bigint
                // (sequences.md §13.1). serial's type is the pseudo-type (always integer), so this
                // only bites an identity column written on a non-integer type.
                if def.identity.is_some() && !ty.is_integer() {
                    return Err(EngineError::new(
                        SqlState::InvalidParameterValue,
                        "identity column type must be smallint, integer, or bigint".to_string(),
                    ));
                }
                // Conflicts (42601, sequences.md §13.2). An explicit DEFAULT — or a `serial` type,
                // itself a synthesized default — alongside IDENTITY is "both default and identity";
                // a `serial` column with its own explicit DEFAULT is "multiple default values" (the
                // S3 message, unchanged).
                if def.identity.is_some() && (def.default.is_some() || serial_kind.is_some()) {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!(
                            "both default and identity specified for column {} of table {}",
                            def.name, ct.name
                        ),
                    ));
                }
                if serial_kind.is_some() && def.default.is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!(
                            "multiple default values specified for column {} of table {}",
                            def.name, ct.name
                        ),
                    ));
                }
                // Create the OWNED sequence — a default ascending i64 for `serial`, or the IDENTITY
                // column's `( seq_options )` (defaulting the same way) — and synthesize the
                // `DEFAULT nextval('<auto-name>')` expression default (format_version 8 mechanism).
                // `SEQUENCE NAME` in the identity options names the sequence (sequences.md §13.1);
                // otherwise PostgreSQL's ChooseRelationName auto-name.
                let explicit_name = def
                    .identity
                    .as_ref()
                    .and_then(|id| id.options.sequence_name.clone());
                let seqname = match explicit_name {
                    Some(n) => {
                        check_reserved_name("sequence", &n)?;
                        if self.relation_exists(&n)
                            || n == ct.name
                            || pending_serials.iter().any(|s| s.name == n)
                        {
                            return Err(EngineError::new(
                                SqlState::DuplicateTable,
                                format!("relation already exists: {n}"),
                            ));
                        }
                        n
                    }
                    None => self.choose_serial_seq_name(&ct.name, &def.name, &pending_serials, &[]),
                };
                let owner = SeqOwner {
                    table: ct.name.clone(),
                    column: columns.len() as u16, // this column's ordinal (pushed below)
                };
                let mut opts = def
                    .identity
                    .as_ref()
                    .map(|id| id.options.clone())
                    .unwrap_or_default();
                // The owned sequence's data type follows the column (§14): `serial` → the
                // pseudo-type, identity → the column type. An explicit `AS` inside the identity
                // `( … )` options conflicts with that — 42601 (PG: "conflicting or redundant
                // options"). `serial` carries no parsed options, so this only fires for identity.
                if opts.data_type.is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        "conflicting or redundant options".to_string(),
                    ));
                }
                opts.sequence_name = None;
                let seq_scalar = serial_kind.unwrap_or_else(|| ty.scalar());
                opts.data_type = Some(
                    SeqDataType::for_scalar(seq_scalar)
                        .expect("serial / identity column is i16/i32/i64")
                        .pg_name()
                        .to_string(),
                );
                pending_serials.push(build_sequence_def(&seqname, &opts, Some(owner))?);
                // Build the synthetic default exactly as the parser would render the equivalent
                // `DEFAULT nextval('<seqname>')` (space-joined tokens — the canonical expression-text
                // form), so the in-memory expr matches what reload re-parses (constraints.md §2). The
                // sequence name is quoted inside the text when it is not a plain lowercase identifier
                // (grammar.md §3), so the regclass-style parse resolves it exactly.
                let expr_text = nextval_default_text(&seqname);
                let expr = crate::parser::parse_expression(&expr_text)?;
                let identity_kind = def.identity.as_ref().map(|id| {
                    if id.always {
                        IdentityKind::Always
                    } else {
                        IdentityKind::ByDefault
                    }
                });
                (
                    None,
                    Some(DefaultExpr {
                        expr_text,
                        expr,
                        unparsed: false,
                    }),
                    identity_kind,
                )
            } else if ty.is_composite() || ty.is_array() || ty.is_range() {
                // A DEFAULT on a composite-, array-, or range-typed column is not supported this
                // slice (composite.md §12 / array.md §12 / ranges.md §8), except a bare `DEFAULT
                // NULL`: it is the same as no default (PostgreSQL stores none for it), so it records
                // nothing.
                if def
                    .default
                    .as_ref()
                    .is_some_and(|d| !is_bare_null_default(d))
                {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a DEFAULT on a composite-, array-, or range-typed column is not supported yet"
                            .to_string(),
                    ));
                }
                (None, None, None)
            } else {
                let sty = ty.scalar();
                // A clock-relative date string DEFAULT ('today'/'now'/…) must NOT fold at CREATE
                // TABLE: it routes to the EXPRESSION path below, re-resolved to the STABLE
                // DateClock node and evaluated per INSERT — where PostgreSQL folds the literal to
                // the table-creation day, the documented fold-footgun divergence (date.md §6).
                // 'epoch' and every ordinary date string stay foldable constants.
                let clock_default = sty.is_date()
                    && matches!(&def.default, Some(d)
                        if matches!(&d.expr, Expr::Literal(Literal::Text(s), _)
                            if crate::date::date_clock_is_relative(s)));
                match &def.default {
                    None => (None, None, None),
                    Some(d) => match &d.expr {
                        Expr::Literal(lit, _) if !clock_default => (
                            Some(
                                store_value(
                                    literal_to_value_for(lit, sty)?,
                                    sty,
                                    Typmod {
                                        decimal,
                                        varchar_len,
                                        temporal,
                                    },
                                    false,
                                    &def.name,
                                    &self.input_zone(),
                                )
                                .map_err(assignment_hint)?,
                            ),
                            None,
                            None,
                        ),
                        _ => {
                            reject_default_structure(&d.expr)?;
                            let scope = Scope::empty(self);
                            let (_, rty) = resolve(
                                &scope,
                                &d.expr,
                                Some(sty),
                                &mut AggCtx::Forbidden,
                                &mut ParamTypes::default(),
                            )?;
                            if !rty.assignable_to(sty) {
                                return Err(assignment_error(format!(
                                    "column {} is of type {} but default expression is of type {}",
                                    def.name,
                                    sty.canonical_name(),
                                    rty.type_name(),
                                )));
                            }
                            (
                                None,
                                Some(DefaultExpr {
                                    expr_text: d.text.clone(),
                                    expr: d.expr.clone(),
                                    unparsed: false,
                                }),
                                None,
                            )
                        }
                    },
                }
            };
            // The column's effective collation, frozen now (spec/design/collation.md §1). An explicit
            // `COLLATE "name"` is text / text[] only (42804 otherwise, PG-matching) and must name a
            // loaded collation or `C` (42704); a collatable column without a clause inherits the
            // per-database default. A `C` effective collation stores as `None` (the fast path).
            let collation: Option<String> = if let Some(name) = &def.collation {
                if !ty.is_collatable() {
                    return Err(type_error(format!(
                        "collations are not supported by type {}",
                        ty.canonical_name()
                    )));
                }
                resolve_collation_name(self, name)?; // validates loaded; 42704 if not
                if name == "C" {
                    None
                } else {
                    Some(name.clone())
                }
            } else if ty.is_collatable() {
                self.read_snap().default_collation().map(str::to_string)
            } else {
                None
            };
            columns.push(Column {
                name: def.name.clone(),
                ty,
                decimal,
                varchar_len,
                temporal,
                primary_key: def.primary_key,
                // PRIMARY KEY ⇒ NOT NULL; a `serial` or IDENTITY column is NOT NULL too
                // (sequences.md §12/§13).
                not_null: def.primary_key
                    || def.not_null
                    || serial_kind.is_some()
                    || def.identity.is_some(),
                default,
                default_expr,
                identity: identity_kind,
                collation,
                no_collation: false,
                collation_conflict: false,
            });
        }

        // Table-level `PRIMARY KEY (a, b, …)` constraints (constraints.md §3). Check order
        // mirrors PostgreSQL (oracle-probed): a second primary key is 42P16 before its
        // members resolve; members resolve left to right (42703 unknown, 42701 repeated).
        // The LIST order is the KEY order — it may differ from declaration order (the v5
        // catalog persists the ordinal list; the old 0A000 narrowing is lifted). The
        // per-member key-type gate (0A000) remains.
        for pk_list in &ct.table_pks {
            if pk_seen {
                return Err(EngineError::new(
                    SqlState::InvalidTableDefinition,
                    format!(
                        "multiple primary keys for table {} are not allowed",
                        ct.name
                    ),
                ));
            }
            pk_seen = true;
            let mut indices: Vec<usize> = Vec::with_capacity(pk_list.len());
            for name in pk_list {
                let idx = columns
                    .iter()
                    .position(|c: &Column| c.name == *name)
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!("column {name} named in key does not exist"),
                        )
                    })?;
                if indices.contains(&idx) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column {name} appears twice in primary key constraint"),
                    ));
                }
                indices.push(idx);
            }
            for &i in &indices {
                let ty = &columns[i].ty;
                if !self.type_is_keyable(ty) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!("a {} primary key is not supported yet", ty.canonical_name()),
                    ));
                }
                columns[i].primary_key = true;
                columns[i].not_null = true; // PRIMARY KEY ⇒ NOT NULL, per member
            }
            pk = indices;
        }

        // UNIQUE constraints (constraints.md §5.1): resolve members in textual definition
        // order, AFTER the PRIMARY KEY constraints and BEFORE any CHECK validates (PG's
        // order, oracle-probed — transformIndexConstraint runs first). Each member must
        // exist (42703, PG's "named in key" wording), appear once (42701), and be of a
        // key-encodable type (0A000 — the same narrowing as a PK member / index key column;
        // unlike a PK member it stays nullable). Folding + naming happen LAST (after check
        // naming), mirroring PG's index_create-at-execution timing.
        let mut runiques: Vec<(Option<String>, Vec<usize>)> = Vec::with_capacity(ct.uniques.len());
        for u in &ct.uniques {
            let mut indices: Vec<usize> = Vec::with_capacity(u.columns.len());
            for cname in &u.columns {
                let idx = columns
                    .iter()
                    .position(|c: &Column| c.name == *cname)
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!("column {cname} named in key does not exist"),
                        )
                    })?;
                if indices.contains(&idx) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column {cname} appears twice in unique constraint"),
                    ));
                }
                indices.push(idx);
            }
            for &i in &indices {
                let ty = &columns[i].ty;
                if !self.type_is_keyable(ty) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "a {} unique constraint member is not supported yet",
                            ty.canonical_name()
                        ),
                    ));
                }
            }
            runiques.push((u.name.clone(), indices));
        }

        // CHECK constraints (constraints.md §4). All validation runs first, in textual
        // definition order, AFTER the PRIMARY KEY constraints resolved (PG's order,
        // oracle-probed); naming follows in a second pass, so a 42703 in a later check
        // fires before a 42710 between earlier ones. Resolution needs a catalog `Table`,
        // so build it now (checks attach below, before `put_table`).
        let mut table = Table {
            name: ct.name,
            columns,
            pk,
            checks: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            exclusions: Vec::new(),
            // A temp table's end-of-transaction action (42P16 above for any other) — temp-tables.md §5.1.
            on_commit: ct.on_commit.unwrap_or_default(),
            scalar_func: false,
        };
        let mut check_deps: Vec<Vec<HostFuncDep>> = Vec::with_capacity(ct.checks.len());
        for def in &ct.checks {
            // Structural rejections first (a single pre-walk — a documented micro-order
            // divergence from PG, which interleaves them with name/type resolution):
            // subquery 0A000, aggregate 42803, bind parameter 42P02 (constraints.md §4.1).
            reject_check_structure(&def.expr)?;
            let scope = Scope::single(self, &table);
            let (node, ty) = resolve(
                &scope,
                &def.expr,
                None,
                &mut AggCtx::Forbidden,
                &mut ParamTypes::default(),
            )?;
            match ty {
                ResolvedType::Bool | ResolvedType::Null => {}
                ResolvedType::Int(_)
                | ResolvedType::Text
                | ResolvedType::Decimal
                | ResolvedType::Bytea
                | ResolvedType::Uuid
                | ResolvedType::Timestamp
                | ResolvedType::Timestamptz
                | ResolvedType::Date
                | ResolvedType::Interval
                | ResolvedType::Float(_)
                | ResolvedType::Json
                | ResolvedType::Jsonb
                | ResolvedType::JsonPath
                | ResolvedType::Composite(_)
                | ResolvedType::Array(_)
                | ResolvedType::Range(_) => {
                    return Err(type_error("argument of CHECK must be boolean")
                        .with_position(def.expr.location().0));
                }
            }
            // Host functions the check calls (spec/design/extensibility.md §7.1.2): each needs a
            // component identity (42P17), and the resolved dependencies persist with the check.
            check_deps.push(collect_check_host_deps(
                &node,
                self.session.env.functions(),
            )?);
        }
        // Naming (constraints.md §4.3): a single pass in textual order. An explicit name is
        // used as written; a derived name is built from the exact table/column names —
        // `<table>_<col>_check` when the expression references exactly one distinct column,
        // else `<table>_check` — suffixed with the smallest positive integer that frees it.
        // A collision (byte-exact; bare names are already folded) is 42710; derived names never yield to a
        // later explicit one (oracle-probed).
        let mut checks: Vec<CheckConstraint> = Vec::with_capacity(ct.checks.len());
        for (def, host_deps) in ct.checks.iter().zip(check_deps) {
            let name = match &def.name {
                Some(n) => {
                    if checks.iter().any(|c| c.name == *n) {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n.clone()
                }
                None => {
                    let cols = check_referenced_columns(&def.expr, &table.columns);
                    let column = match cols.as_slice() {
                        [i] => Some(table.columns[*i].name.to_string()),
                        _ => None,
                    };
                    // A derived name also walks past every constraint name of the table's
                    // database domain — its schema (constraints.md §6.7).
                    let domain = self.create_table_domain(target_temp, attach_name.as_deref());
                    choose_object_name(&table.name, column.as_deref(), "check", |n| {
                        checks.iter().any(|c| c.name == n)
                            || schema_constraint_name_taken(domain, "", n)
                    })
                }
            };
            checks.push(CheckConstraint {
                name,
                expr_text: def.text.clone(),
                expr: def.expr.clone(),
                unparsed: false,
                host_deps,
            });
        }
        // Evaluation (and on-disk) order: ascending byte order of the exact name
        // (constraints.md §4.4 — PG evaluates checks sorted by name, oracle-probed).
        checks.sort_by_key(|c| c.name.to_string());
        table.checks = checks;

        // UNIQUE fold + naming (constraints.md §5.2/§5.3, PG-probed). Fold first: a
        // constraint whose member list equals the primary key's (same order) creates
        // nothing; identical lists fold into the first occurrence, the surviving name being
        // the first explicitly-named one's. Then each survivor names its backing index in
        // textual order: an explicit name checks the relation namespace (42P07 — existing
        // relations, the table being created, and the statement's earlier indexes) before
        // the table's constraint names (42710); a derived `<table>_<cols>_key` suffix-walks
        // past BOTH namespaces.
        let mut survivors: Vec<(Option<String>, Vec<usize>)> = Vec::new();
        for (uname, cols) in runiques {
            if cols == table.pk {
                continue;
            }
            if let Some(existing) = survivors.iter_mut().find(|(_, c)| *c == cols) {
                if existing.0.is_none() {
                    existing.0 = uname;
                }
                continue;
            }
            survivors.push((uname, cols));
        }
        for (uname, cols) in survivors {
            let taken = |exec: &Self, t: &Table, n: &str| {
                exec.relation_exists(n) || t.name == n || t.indexes.iter().any(|i| i.name == n)
            };
            let name = match uname {
                Some(n) => {
                    // A named UNIQUE constraint IS its backing index (constraints.md §5), so the
                    // user-written name enters the relation namespace — reserved-prefix checked
                    // like any relation name (introspection.md §4).
                    check_reserved_name("constraint", &n)?;
                    if taken(self, &table, &n) {
                        return Err(EngineError::new(
                            SqlState::DuplicateTable,
                            format!("relation already exists: {n}"),
                        ));
                    }
                    if table.checks.iter().any(|c| c.name == n) {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n
                }
                None => {
                    let names: Vec<&str> = cols.iter().map(|&i| &*table.columns[i].name).collect();
                    let addition = name_addition(&names);
                    let domain = self.create_table_domain(target_temp, attach_name.as_deref());
                    choose_object_name(&table.name, Some(&addition), "key", |n| {
                        taken(self, &table, n)
                            || table.checks.iter().any(|c| c.name == n)
                            || schema_constraint_name_taken(domain, "", n)
                    })
                }
            };
            // Insert in catalog (ascending name byte) order — indexes.md §6.
            let name_key = name.to_string();
            let pos = table
                .indexes
                .iter()
                .position(|i| i.name > name_key)
                .unwrap_or(table.indexes.len());
            table.indexes.insert(
                pos,
                IndexDef {
                    name,
                    keys: cols.into_iter().map(IndexKey::Column).collect(),
                    unique: true,
                    kind: IndexKind::Btree,
                    predicate: None,
                    // A PK / UNIQUE constraint index is always plain-column — no host dependency.
                    host_deps: Vec::new(),
                    timezone_deps: TimeZoneDeps::default(),
                },
            );
        }

        // FOREIGN KEY constraints (constraints.md §6). Resolved AFTER the PK / UNIQUE / CHECK
        // constraints (PG's order), each in textual definition order: resolve the local columns
        // (42703/42701) against this table; look up the parent (42P01, or the table itself for a
        // self-reference); resolve the referenced columns (default to the parent PK, 42830 if it
        // has none); check the arity (42830); name the constraint (explicit collision 42710, else
        // derive `<table>_<cols>_fkey` with a suffix walk through the constraint namespace);
        // resolve the selected actions; require the referenced columns to be the
        // parent PK or a UNIQUE set (42830); and require same-type pairing (42804, stricter than
        // PG). An FK owns no B-tree — enforcement probes the parent at every write (§6.4/§6.5).
        let mut resolved_fks: Vec<ForeignKeyConstraint> = Vec::with_capacity(ct.foreign_keys.len());
        for fk in &ct.foreign_keys {
            // 1. Local (referencing) columns into this table.
            let mut local: Vec<usize> = Vec::with_capacity(fk.columns.len());
            for cname in &fk.columns {
                let idx = table
                    .columns
                    .iter()
                    .position(|c| c.name == *cname)
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!("column {cname} named in key does not exist"),
                        )
                    })?;
                if local.contains(&idx) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column {cname} appears twice in foreign key constraint"),
                    ));
                }
                local.push(idx);
            }
            // 2. Parent table — a self-reference resolves against the in-progress definition;
            // any other name takes the temp-first resolution walk, and the parent must be of the
            // child's own kind (42P16, PostgreSQL's ATAddForeignKeyConstraint): an FK never crosses
            // from the session-local temp domain to the main database or back (temp-tables.md §8).
            let self_ref = fk.ref_table == table.name;
            let parent: &Table = if self_ref {
                &table
            } else {
                let parent = self
                    .table(&fk.ref_table)
                    .ok_or_else(|| fk_parent_missing(self.read_snap(), &fk.ref_table))?;
                check_fk_persistence(target_temp, self.is_temp_table(&fk.ref_table))?;
                parent
            };
            // 3. Referenced columns into the parent (default to the parent's primary key).
            let refs: Vec<usize> = match &fk.ref_columns {
                None => {
                    if parent.pk.is_empty() {
                        // Omitting the referenced list defaults to the parent's PRIMARY KEY; a
                        // parent without one is 42704 (PG's code here — undefined_object — even
                        // when the parent has a UNIQUE), distinct from the explicit-no-match 42830.
                        return Err(EngineError::new(
                            SqlState::UndefinedObject,
                            format!(
                                "there is no primary key for referenced table {}",
                                parent.name
                            ),
                        ));
                    }
                    parent.pk.clone()
                }
                Some(cols) => {
                    let mut r: Vec<usize> = Vec::with_capacity(cols.len());
                    for cname in cols {
                        let idx = parent
                            .columns
                            .iter()
                            .position(|c| c.name == *cname)
                            .ok_or_else(|| {
                                EngineError::new(
                                    SqlState::UndefinedColumn,
                                    format!("column {cname} named in key does not exist"),
                                )
                            })?;
                        if r.contains(&idx) {
                            return Err(EngineError::new(
                                SqlState::DuplicateColumn,
                                format!("column {cname} appears twice in foreign key constraint"),
                            ));
                        }
                        r.push(idx);
                    }
                    r
                }
            };
            // 4. Referencing/referenced count must agree.
            if local.len() != refs.len() {
                return Err(EngineError::new(
                    SqlState::InvalidForeignKey,
                    "number of referencing and referenced columns for foreign key disagree"
                        .to_string(),
                ));
            }
            // 5. Name — the per-table constraint namespace: the CHECK and UNIQUE constraints
            // already defined (PostgreSQL creates the indexes before the FKs) and the earlier FKs
            // (§6.2/§6.7). A derived name also walks past the domain's other tables' names.
            let local_taken = |n: &str| {
                table.checks.iter().any(|c| c.name == n)
                    || table.indexes.iter().any(|i| i.unique && i.name == n)
                    || resolved_fks.iter().any(|f| f.name == n)
            };
            let name = match &fk.name {
                Some(n) => {
                    if local_taken(n) {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n.clone()
                }
                None => {
                    let names: Vec<&str> = local.iter().map(|&i| &*table.columns[i].name).collect();
                    let addition = name_addition(&names);
                    let domain = self.create_table_domain(target_temp, attach_name.as_deref());
                    choose_object_name(&table.name, Some(&addition), "fkey", |n| {
                        local_taken(n) || schema_constraint_name_taken(domain, "", n)
                    })
                }
            };
            // 6. Resolve and store the selected referential actions (§6.6).
            let on_delete = fk_action(fk.on_delete, "DELETE")?;
            let on_update = fk_action(fk.on_update, "UPDATE")?;
            // 7. The referenced columns must be the parent's PK or a UNIQUE set (§6.2).
            let ref_set = sorted_unique(&refs);
            let matches_unique = (!parent.pk.is_empty() && sorted_unique(&parent.pk) == ref_set)
                || parent.indexes.iter().any(|i| {
                    i.unique
                        && i.column_ordinals()
                            .is_some_and(|c| sorted_unique(&c) == ref_set)
                });
            if !matches_unique {
                return Err(EngineError::new(
                    SqlState::InvalidForeignKey,
                    format!(
                        "there is no unique constraint matching given keys for referenced table {}",
                        parent.name
                    ),
                ));
            }
            // 8. Same-type pairing (§6.2). Because the referenced columns are a PK/UNIQUE key they
            // are key-encodable, so a same-typed local column is key-encodable too — no separate
            // 0A000 type gate is needed.
            for (li, ri) in local.iter().zip(&refs) {
                if table.columns[*li].ty != parent.columns[*ri].ty {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        format!(
                            "foreign key constraint {name} cannot be implemented: key columns {} and {} are of incompatible types: {} and {}",
                            table.columns[*li].name,
                            parent.columns[*ri].name,
                            table.columns[*li].ty.canonical_name(),
                            parent.columns[*ri].ty.canonical_name(),
                        ),
                    ));
                }
            }
            resolved_fks.push(ForeignKeyConstraint {
                name,
                columns: local,
                ref_table: parent.name.clone(),
                ref_columns: refs,
                match_full: fk.match_full,
                on_delete,
                on_update,
            });
        }
        // Held in ascending name byte order (the catalog's on-disk + evaluation order, §6.9).
        resolved_fks.sort_by_key(|f| f.name.to_string());
        table.foreign_keys = resolved_fks;

        // EXCLUDE constraints (spec/design/gist.md §7). Resolved AFTER the PK / UNIQUE / CHECK / FK
        // constraints, each in textual order: resolve the element columns (42703/42701) and the
        // `WITH` operators against the column types (42704 no-opclass / 0A000 deferred-or-unsupported),
        // name the constraint + its backing GiST index (the constraint IS its index — they share a
        // name; 42P07/42710 across the relation + constraint namespaces), and build the
        // **multi-column** GiST index that enforces it. The probe + `23P01` live in INSERT/UPDATE.
        for exc in &ct.excludes {
            let (indices, elements) =
                resolve_exclusion_definition(&table, exc.using.as_deref(), &exc.elements)?;
            // Name the constraint (= its backing index name). An explicit name checks the relation
            // namespace (42P07) then the table's constraint names (42710); a derived
            // `<table>_<cols>_excl` suffix-walks both.
            let taken = |exec: &Self, t: &Table, n: &str| {
                exec.relation_exists(n) || t.name == n || t.indexes.iter().any(|i| i.name == n)
            };
            let constraint_taken = |t: &Table, n: &str| {
                t.checks.iter().any(|c| c.name == n)
                    || t.foreign_keys.iter().any(|f| f.name == n)
                    || t.exclusions.iter().any(|e| e.name == n)
            };
            let name = match &exc.name {
                Some(n) => {
                    // The named EXCLUDE constraint's backing GiST index carries the user-written
                    // name into the relation namespace (introspection.md §4).
                    check_reserved_name("constraint", n)?;
                    if taken(self, &table, n) {
                        return Err(EngineError::new(
                            SqlState::DuplicateTable,
                            format!("relation already exists: {n}"),
                        ));
                    }
                    if constraint_taken(&table, n) {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n.clone()
                }
                None => {
                    let names: Vec<&str> =
                        indices.iter().map(|&i| &*table.columns[i].name).collect();
                    let addition = name_addition(&names);
                    let domain = self.create_table_domain(target_temp, attach_name.as_deref());
                    choose_object_name(&table.name, Some(&addition), "excl", |n| {
                        taken(self, &table, n)
                            || constraint_taken(&table, n)
                            || schema_constraint_name_taken(domain, "", n)
                    })
                }
            };
            // Insert the backing GiST index in catalog (ascending name byte) order.
            let name_key = name.to_string();
            let pos = table
                .indexes
                .iter()
                .position(|i| i.name > name_key)
                .unwrap_or(table.indexes.len());
            table.indexes.insert(
                pos,
                IndexDef {
                    name: name.clone(),
                    keys: indices.into_iter().map(IndexKey::Column).collect(),
                    unique: false,
                    kind: IndexKind::Gist,
                    predicate: None,
                    // A GiST exclusion-backing index is plain-column — no host dependency.
                    host_deps: Vec::new(),
                    timezone_deps: TimeZoneDeps::default(),
                },
            );
            table.exclusions.push(ExclusionConstraint {
                name: name.clone(),
                index: name,
                elements,
            });
        }
        // Held in ascending name byte order (the catalog's on-disk order — gist.md §8).
        table.exclusions.sort_by_key(|e| e.name.to_string());

        let index_keys: Vec<String> = table.indexes.iter().map(|i| i.name.to_string()).collect();
        // The table is brand new (no rows), so each backing index store starts empty.
        let cap = crate::format::page_payload(self.page_size);

        if let Some(name) = attach_name.clone() {
            // Deferred narrowings on an attached-database table this slice (attached-databases.md §8),
            // each a clean 0A000: a COMPOSITE-typed column (its type lives in the MAIN catalog — no
            // cross-scope type reference this slice), a serial/IDENTITY column (its OWNED sequence would
            // be a cross-scope sequence), and a collated column (the attachment snapshot carries no
            // collation catalog). Plain scalar / array / range / decimal columns with PK / NOT NULL /
            // DEFAULT / CHECK / UNIQUE and secondary btree indexes are fully supported.
            for c in &table.columns {
                if c.ty.is_composite() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a composite-typed column on an attached-database table is not supported yet",
                    ));
                }
                if c.collation.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "COLLATE on an attached-database-table column {} is not yet supported",
                            c.name
                        ),
                    ));
                }
            }
            if !pending_serials.is_empty() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a serial / IDENTITY column on an attached-database table is not supported yet",
                ));
            }
            // Resolve each column's `ColType` against the MAIN snapshot's composite-type catalog (as for
            // temp) — attachment tables carry no composite column (gated above), so this is trivially
            // scalar, but the resolved tree is self-contained regardless (composite.md §4).
            let col_types: Vec<ColType> = {
                let main = self.read_snap();
                table
                    .columns
                    .iter()
                    .map(|c| resolve_col_type(&c.ty, &main.types))
                    .collect()
            };
            // Build the attachment's new stores at ITS OWN page size (§2) — a file attachment may
            // serialize at a different page size than main, and its records must split to match.
            let ps = self.attach_page_size(&name);
            let acap = crate::format::page_payload(ps);
            // Register into the attachment's working snapshot (attached-databases.md §6) — never the main
            // image; published into `Roots::attached` at commit (N-root commit, §5). `attach_write_snap`
            // clones the attachment's committed root (which already carries its `store_paging`) on first
            // write and marks it dirty, so its NEW stores bind to the attachment's own paging.
            let ws = self.attach_write_snap(&name);
            ws.put_table_resolved(table, col_types, ps);
            for k in index_keys {
                ws.put_index_store(k, TableStore::new(acap, Vec::new()));
            }
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
        }

        if target_temp {
            // Deferred narrowing on a temp table this slice (spec/design/temp-tables.md §8), a clean
            // 0A000: a collated column (needs the temp snapshot to carry the collation catalog). Plain
            // scalar/array/range/decimal columns with PK / NOT NULL / DEFAULT / CHECK / UNIQUE,
            // `serial`/IDENTITY columns (the OWNED sequence is staged into the same temp snapshot
            // below), and COMPOSITE-typed columns (resolved against the MAIN type catalog just below)
            // are fully supported.
            if let Some(c) = table.columns.iter().find(|c| c.collation.is_some()) {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "COLLATE on temporary-table column {} is not yet supported",
                        c.name
                    ),
                ));
            }
            // Resolve each column's `ColType` against the MAIN snapshot's composite-type catalog
            // (spec/design/temp-tables.md §8): composites are always persistent (`CREATE TYPE` is
            // persistent DDL), so the temp snapshot's own `types` map is empty — resolving there would
            // panic on a composite reference. Done here, against `read_snap()`, BEFORE the temp mutable
            // borrow; the resulting `ColType` tree is self-contained, so the temp store needs nothing
            // from the catalog after this (composite.md §4).
            let col_types: Vec<ColType> = {
                let main = self.read_snap();
                table
                    .columns
                    .iter()
                    .map(|c| resolve_col_type(&c.ty, &main.types))
                    .collect()
            };
            // Register into the session-local temp snapshot — never the main image, so the table makes
            // zero file writes (§2). page_size only weighs records for the (unused-for-resident) split
            // heuristic.
            let ps = self.page_size;
            // The session-local temp snapshot rides a per-domain `MemoryBlockStore` pager
            // (temp-tables.md §6): lazily create the domain storage and stamp its paging onto the working
            // snapshot, so `put_table_resolved` / `put_index_store` attach it to every temp store.
            let store_paging = Some(self.temp_domain_paging());
            let tw = self.temp_working_mut();
            tw.store_paging = store_paging;
            tw.put_table_resolved(table, col_types, ps);
            for k in index_keys {
                tw.put_index_store(k, TableStore::new(cap, Vec::new()));
            }
            // Stage each `serial`/IDENTITY column's OWNED sequence into the SAME temp snapshot
            // (spec/design/sequences.md §12, temp-tables.md §8) — never the main image, so the
            // sequence (like the table) makes zero file writes and is dropped with the table. The
            // names were resolved collision-free during the column walk (`relation_exists` is
            // temp-aware); `nextval` resolves and advances them via the scope-aware sequence funnel.
            for s in pending_serials {
                tw.put_sequence(s);
            }
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
        }

        self.put_table(table);
        for k in index_keys {
            self.working_mut()
                .put_index_store(k, TableStore::new(cap, Vec::new()));
        }
        // Stage each `serial` column's OWNED sequence now that the table validated
        // (spec/design/sequences.md §12). The names were resolved (collision-free) during the column
        // walk; the table is in the catalog, so a `DROP TABLE` will auto-drop these.
        for s in pending_serials {
            self.working_mut().put_sequence(s);
        }
        // DDL touches no rows and evaluates no expressions: zero cost.
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Execute ALTER TABLE slices 1-5 (alter.md §1-§3.4).
    pub(crate) fn execute_alter_table(&mut self, at: AlterTable) -> Result<Outcome> {
        check_catalog_rel_write(&at.name)?;
        self.check_attachment_writable(at.db.as_deref())?;
        let temp_scope = match at.db.as_deref() {
            Some(q) => q == "temp",
            None => self.is_temp_table(&at.name),
        };
        let snap = match at.db.as_deref().map(str::to_string) {
            None => {
                if self.is_temp_table(&at.name)
                    || self.temp_read_snap().find_index(&at.name).is_some()
                    || self.temp_read_snap().sequence(&at.name).is_some()
                {
                    self.temp_read_snap()
                } else {
                    self.read_snap()
                }
            }
            Some(q) if q == "temp" => self.temp_read_snap(),
            Some(q) if q == "main" => self.read_snap(),
            Some(q) => self.attach_read_snap(&q).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedTable,
                    format!("database \"{}\" is not attached", at.db.as_deref().unwrap()),
                )
            })?,
        }
        .clone();
        let Some(original_ref) = snap.table(&at.name) else {
            // A view: its renames are the view renames (views.md §7.1); every other action is the
            // wrong object kind (42809, views.md §2).
            if snap.view(&at.name).is_some() {
                let action = match at.action {
                    AlterTableAction::RenameTable(new) => Some(AlterViewAction::RenameTo(new)),
                    AlterTableAction::RenameColumn { old, new } => {
                        Some(AlterViewAction::RenameColumn { old, new })
                    }
                    _ => None,
                };
                if let Some(action) = action {
                    return self.execute_alter_view(AlterView {
                        name: at.name,
                        db: at.db,
                        if_exists: at.if_exists,
                        action,
                    });
                }
                return Err(EngineError::new(
                    SqlState::WrongObjectType,
                    format!("{} is not a table", at.name),
                ));
            }
            if snap.find_index(&at.name).is_some() || snap.sequence(&at.name).is_some() {
                return Err(EngineError::new(
                    SqlState::WrongObjectType,
                    format!("{} is not a table", at.name),
                ));
            }
            if at.if_exists {
                return Ok(Outcome::Statement {
                    cost: 0,
                    rows_affected: None,
                });
            }
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", at.name),
            ));
        };
        let original = original_ref.clone();
        let old_key = original.name.to_string();
        let mut table = original.clone();
        let mut rename_table = false;
        let mut index_rename: Option<(String, String)> = None;
        let mut added_constraints = std::collections::HashSet::<String>::new();
        #[derive(Clone)]
        enum ColumnSource {
            Original(usize),
            Added,
        }
        enum RowStep {
            Add {
                column: Column,
                col_type: ColType,
                default: Option<RExpr>,
            },
            Drop(usize),
            SetType {
                column: usize,
                value: RExpr,
            },
        }
        let mut column_sources: Vec<ColumnSource> = original
            .columns
            .iter()
            .enumerate()
            .map(|(i, _)| ColumnSource::Original(i))
            .collect();
        let mut type_changed_columns = vec![false; original.columns.len()];
        let mut row_steps = Vec::<RowStep>::new();
        let mut type_changed = false;
        let mut prep_cost = 0i64;
        let mut pending_serials = Vec::<SequenceDef>::new();
        let mut cascade_unique_cols = Vec::<Vec<usize>>::new();
        let mut cascade_dropped_column_fks = std::collections::HashSet::<(String, String)>::new();
        let attachment_scope = at
            .db
            .as_deref()
            .is_some_and(|q| !matches!(q.to_string().as_str(), "main" | "temp"));
        let main_namespace = self.read_snap().clone();
        let temp_namespace = self.temp_read_snap().clone();
        // Views that depend on the table (spec/design/views.md §6–§8). A main-database table's
        // dependents are captured now — traced against the pre-statement catalog when the action
        // edits their text — and rewritten + re-validated after the change. A table of an attached
        // database with dependent views refuses every ALTER TABLE: re-validating views there is a
        // follow-on (views.md §10).
        let mut view_plan = if temp_scope {
            None
        } else if attachment_scope {
            if let Some(v) = snap
                .views_depending_on(ViewDepKind::Table, &original.name)
                .first()
            {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "cannot alter table {} because view {} depends on it",
                        original.name, v.name
                    ),
                ));
            }
            None
        } else {
            let rewrites = match &at.action {
                AlterTableAction::RenameTable(_) | AlterTableAction::RenameColumn { .. } => true,
                AlterTableAction::Actions(actions) => actions
                    .iter()
                    .any(|a| matches!(a, AlterTableEdit::DropColumn { .. })),
                AlterTableAction::RenameConstraint { .. } => false,
            };
            self.prepare_view_alter(&original, rewrites)?
        };
        let mut view_change: Option<ViewAlterChange> = None;
        // The views a `DROP COLUMN … CASCADE` drops (views.md §6), and the pre-statement ordinals of
        // the columns whose type changes — checked against the surviving views after every action,
        // as PostgreSQL runs type changes after the drops.
        let mut cascade_dropped_views = std::collections::BTreeSet::<String>::new();
        let mut view_type_checks = Vec::<usize>::new();
        let relation_taken = |name: &str| {
            if attachment_scope {
                snap.table(name).is_some()
                    || snap.find_index(name).is_some()
                    || snap.sequence(name).is_some()
                    || snap.view(name).is_some()
            } else {
                main_namespace.table(name).is_some()
                    || main_namespace.find_index(name).is_some()
                    || main_namespace.sequence(name).is_some()
                    || main_namespace.view(name).is_some()
                    || temp_namespace.table(name).is_some()
                    || temp_namespace.find_index(name).is_some()
                    || temp_namespace.sequence(name).is_some()
            }
        };
        let mut dropped_owned_sequences = std::collections::HashSet::<String>::new();
        // ADD/DROP actions edit `table` left-to-right. A dropped index owned by this table has
        // released its relation name, while an earlier ADD has already occupied its new one.
        let working_relation_taken = |working: &Table,
                                      pending_sequences: &[SequenceDef],
                                      dropped_sequences: &std::collections::HashSet<String>,
                                      name: &str| {
            if pending_sequences.iter().any(|s| s.name == name) {
                return true;
            }
            if working.indexes.iter().any(|i| i.name == name) {
                true
            } else if original.indexes.iter().any(|i| i.name == name) {
                false
            } else if dropped_sequences.contains(&name.to_string()) {
                false
            } else {
                relation_taken(name)
            }
        };

        match at.action {
            AlterTableAction::RenameTable(new_name) => {
                check_reserved_name("table", &new_name)?;
                if relation_taken(&new_name) {
                    return Err(EngineError::new(
                        SqlState::DuplicateTable,
                        format!("relation already exists: {new_name}"),
                    ));
                }
                self.rewrite_table_expressions(&mut table, None, Some(&new_name))?;
                view_change = Some(ViewAlterChange::RenameRelation(new_name.clone()));
                table.name = new_name;
                rename_table = true;
            }
            AlterTableAction::RenameColumn { old, new } => {
                let Some(ci) = table.columns.iter().position(|c| c.name == old) else {
                    return Err(EngineError::new(
                        SqlState::UndefinedColumn,
                        format!("column does not exist: {old}"),
                    ));
                };
                if table.columns.iter().any(|c| c.name == new) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column already exists: {new}"),
                    ));
                }
                self.rewrite_table_expressions(&mut table, Some((&old, &new)), None)?;
                view_change = Some(ViewAlterChange::RenameColumn {
                    ordinal: ci,
                    new: new.clone(),
                });
                table.columns[ci].name = new;
            }
            AlterTableAction::RenameConstraint { old, new } => {
                let exists = |name: &str, t: &Table| {
                    t.checks.iter().any(|c| c.name == name)
                        || t.indexes.iter().any(|i| i.unique && i.name == name)
                        || t.foreign_keys.iter().any(|f| f.name == name)
                        || t.exclusions.iter().any(|e| e.name == name)
                };
                if !exists(&old, &table) {
                    return Err(EngineError::new(
                        SqlState::UndefinedObject,
                        format!("constraint does not exist: {old}"),
                    ));
                }
                if exists(&new, &table) {
                    return Err(EngineError::new(
                        SqlState::DuplicateObject,
                        format!("constraint already exists: {new}"),
                    ));
                }
                for c in &mut table.checks {
                    if c.name == old {
                        c.name = new.clone();
                    }
                }
                for f in &mut table.foreign_keys {
                    if f.name == old {
                        f.name = new.clone();
                    }
                }
                for e in &mut table.exclusions {
                    if e.name == old {
                        e.name = new.clone();
                        e.index = new.clone();
                        index_rename = Some((old.clone(), new.clone()));
                    }
                }
                for i in &mut table.indexes {
                    if (i.unique || index_rename.is_some()) && i.name == old {
                        i.name = new.clone();
                        index_rename = Some((old.clone(), new.clone()));
                    }
                }
                if index_rename.is_some() {
                    check_reserved_name("constraint", &new)?;
                    if relation_taken(&new) {
                        return Err(EngineError::new(
                            SqlState::DuplicateTable,
                            format!("relation already exists: {new}"),
                        ));
                    }
                }
                table.checks.sort_by_key(|x| x.name.to_string());
                table.indexes.sort_by_key(|x| x.name.to_string());
                table.foreign_keys.sort_by_key(|x| x.name.to_string());
                table.exclusions.sort_by_key(|x| x.name.to_string());
            }
            AlterTableAction::Actions(actions) => {
                let mut actions = std::collections::VecDeque::from(actions);
                while let Some(action) = actions.pop_front() {
                    if let AlterTableEdit::AddColumn {
                        column,
                        checks,
                        uniques,
                        foreign_keys,
                        if_not_exists,
                    } = action
                    {
                        if table.columns.iter().any(|c| c.name == column.name) {
                            if if_not_exists {
                                continue;
                            }
                            return Err(EngineError::new(
                                SqlState::DuplicateColumn,
                                format!("column already exists: {}", column.name),
                            ));
                        }
                        if !foreign_keys.is_empty() && attachment_scope {
                            return Err(EngineError::new(
                                SqlState::FeatureNotSupported,
                                "this constraint is not supported in this table scope",
                            ));
                        }
                        // Earlier index actions already live in `table`; named inline UNIQUE
                        // constraints are queued only after this column resolves. Both reserve their
                        // relation names while an owned serial/identity sequence chooses its name.
                        let mut reserved_relation_names: Vec<String> =
                            table.indexes.iter().map(|i| i.name.clone()).collect();
                        if !column.primary_key {
                            reserved_relation_names
                                .extend(uniques.iter().filter_map(|u| u.name.clone()));
                        }
                        let col = self.build_alter_added_column(
                            &table,
                            &column,
                            &mut pending_serials,
                            &reserved_relation_names,
                            temp_scope,
                            attachment_scope,
                        )?;
                        table.columns.push(col.clone());
                        let added_primary_key = col.primary_key;
                        if added_primary_key {
                            table.pk = vec![table.columns.len() - 1];
                        }
                        column_sources.push(ColumnSource::Added);
                        type_changed_columns.push(false);
                        let added = table.columns.last().unwrap().clone();
                        let default = self
                            .resolve_default_exprs(&Table {
                                name: table.name.clone(),
                                columns: vec![added.clone()],
                                pk: Vec::new(),
                                checks: Vec::new(),
                                indexes: Vec::new(),
                                foreign_keys: Vec::new(),
                                exclusions: Vec::new(),
                                on_commit: crate::catalog::OnCommit::PreserveRows,
                                scalar_func: false,
                            })?
                            .pop()
                            .unwrap();
                        row_steps.push(RowStep::Add {
                            col_type: snap.resolve_col_type(&added.ty),
                            column: added,
                            default,
                        });
                        // Inline constraints are the same definitions as CREATE TABLE. Queue them
                        // immediately after the column so later comma-actions see their effects.
                        for fk in foreign_keys.into_iter().rev() {
                            actions.push_front(AlterTableEdit::AddConstraint(
                                AlterConstraintDef::ForeignKey(fk),
                            ));
                        }
                        for unique in uniques.into_iter().filter(|_| !added_primary_key).rev() {
                            actions.push_front(AlterTableEdit::AddConstraint(
                                AlterConstraintDef::Unique(unique),
                            ));
                        }
                        for check in checks.into_iter().rev() {
                            actions.push_front(AlterTableEdit::AddConstraint(
                                AlterConstraintDef::Check(check),
                            ));
                        }
                        continue;
                    }
                    if let AlterTableEdit::DropColumn {
                        name,
                        if_exists,
                        cascade,
                    } = action
                    {
                        let Some(ci) = table.columns.iter().position(|c| c.name == name) else {
                            if if_exists {
                                continue;
                            }
                            return Err(EngineError::new(
                                SqlState::UndefinedColumn,
                                format!("column does not exist: {name}"),
                            ));
                        };
                        if table.pk.contains(&ci) {
                            return Err(EngineError::new(
                                SqlState::FeatureNotSupported,
                                "dropping a primary-key column is not supported yet".to_string(),
                            ));
                        }
                        // A column a view references cannot be dropped (views.md §6): 2BP01. Under
                        // CASCADE those views are dropped, with every view depending on them.
                        if let (ColumnSource::Original(old_ci), Some(vp)) =
                            (&column_sources[ci], &mut view_plan)
                        {
                            let users: Vec<String> =
                                vp.views_using(*old_ci).map(str::to_string).collect();
                            if let Some(view) = users.first()
                                && !cascade
                            {
                                return Err(EngineError::new(
                                    SqlState::DependentObjectsStillExist,
                                    format!(
                                        "cannot drop column {name} of table {} because view {view} depends on it",
                                        original.name
                                    ),
                                )
                                .hinted(error_hint::DROP_CASCADE));
                            }
                            if !users.is_empty() {
                                let closure = main_namespace.view_drop_closure(users);
                                vp.forget(&closure);
                                cascade_dropped_views.extend(closure);
                            }
                        }
                        // Which checks and indexes reference the column is read from their parsed
                        // expressions, so an unparsed one blocks the drop
                        // (spec/design/unparsable-definitions.md §3.2/§3.4).
                        table.unparsed_error(false)?;
                        let dropped_source = column_sources[ci].clone();
                        // PostgreSQL owns an FK through its local (referencing) columns. Dropping
                        // one removes the whole FK under RESTRICT. A self-FK that uses the column
                        // only on the referenced side remains a dependency.
                        let table_name = table.name.clone();
                        let fk_uses_local_column =
                            |fk: &ForeignKeyConstraint| fk.columns.contains(&ci);
                        let fk_uses_self_ref_column = |fk: &ForeignKeyConstraint| {
                            fk.ref_table == table_name && fk.ref_columns.contains(&ci)
                        };
                        let self_ref_fk_dep = table
                            .foreign_keys
                            .iter()
                            .any(|fk| fk_uses_self_ref_column(fk) && !fk_uses_local_column(fk));
                        let incoming_dep = match column_sources[ci] {
                            ColumnSource::Original(old_ci) => snap.tables.iter().any(|(key, t)| {
                                key != &old_key
                                    && t.foreign_keys.iter().any(|fk| {
                                        fk.ref_table == original.name
                                            && fk.ref_columns.contains(&old_ci)
                                            && !cascade_dropped_column_fks
                                                .contains(&(key.to_string(), fk.name.to_string()))
                                            && !cascade_unique_cols
                                                .iter()
                                                .any(|cols| *cols == sorted_unique(&fk.ref_columns))
                                    })
                            }),
                            ColumnSource::Added => false,
                        };
                        if !cascade && (self_ref_fk_dep || incoming_dep) {
                            return Err(EngineError::new(
                                SqlState::DependentObjectsStillExist,
                                format!(
                                    "cannot drop column {name} because other objects depend on it"
                                ),
                            )
                            .hinted(error_hint::DROP_CASCADE));
                        }
                        if cascade {
                            if let ColumnSource::Original(old_ci) = column_sources[ci] {
                                for (key, child) in snap.tables.iter() {
                                    if key == &old_key {
                                        continue;
                                    }
                                    for fk in &child.foreign_keys {
                                        if fk.ref_table == original.name
                                            && fk.ref_columns.contains(&old_ci)
                                        {
                                            cascade_dropped_column_fks
                                                .insert((key.to_string(), fk.name.to_string()));
                                        }
                                    }
                                }
                            }
                        }
                        // PostgreSQL treats same-table CHECKs, indexes (including UNIQUE), and
                        // EXCLUDE constraints as internally dependent on their columns. They
                        // disappear even under RESTRICT; CASCADE is for external dependents.
                        let mut removed = Vec::<String>::new();
                        table.checks.retain(|c| {
                            let keep =
                                !check_referenced_columns(&c.expr, &table.columns).contains(&ci);
                            if !keep {
                                removed.push(c.name.to_string());
                            }
                            keep
                        });
                        table.indexes.retain(|ix| {
                            let uses = ix.keys.iter().any(|k| match k {
                                IndexKey::Column(c) => *c == ci,
                                IndexKey::Expr(e) => {
                                    check_referenced_columns(&e.expr, &table.columns).contains(&ci)
                                }
                            }) || ix.predicate.as_ref().is_some_and(|p| {
                                check_referenced_columns(&p.expr, &table.columns).contains(&ci)
                            });
                            if uses {
                                removed.push(ix.name.to_string());
                            }
                            !uses
                        });
                        table.exclusions.retain(|e| {
                            let uses = e.elements.iter().any(|x| x.column == ci);
                            if uses {
                                removed.push(e.name.to_string());
                            }
                            !uses
                        });
                        for name in removed {
                            added_constraints.remove(&name);
                        }
                        // A local-side FK is internally owned by the dropped column and disappears
                        // without CASCADE. CASCADE also removes referenced-side-only self-FKs.
                        let mut removed_fks = Vec::<String>::new();
                        table.foreign_keys.retain(|fk| {
                            let remove = fk_uses_local_column(fk)
                                || (cascade && fk_uses_self_ref_column(fk));
                            if remove {
                                removed_fks.push(fk.name.to_string());
                            }
                            !remove
                        });
                        for name in removed_fks {
                            added_constraints.remove(&name);
                        }
                        table.columns.remove(ci);
                        column_sources.remove(ci);
                        type_changed_columns.remove(ci);
                        row_steps.push(RowStep::Drop(ci));
                        for p in &mut table.pk {
                            if *p > ci {
                                *p -= 1;
                            }
                        }
                        for ix in &mut table.indexes {
                            for key in &mut ix.keys {
                                if let IndexKey::Column(c) = key {
                                    if *c > ci {
                                        *c -= 1;
                                    }
                                }
                            }
                        }
                        for fk in &mut table.foreign_keys {
                            for c in &mut fk.columns {
                                if *c > ci {
                                    *c -= 1;
                                }
                            }
                            if fk.ref_table == table.name {
                                for c in &mut fk.ref_columns {
                                    if *c > ci {
                                        *c -= 1;
                                    }
                                }
                            }
                        }
                        for ex in &mut table.exclusions {
                            for el in &mut ex.elements {
                                if el.column > ci {
                                    el.column -= 1;
                                }
                            }
                        }
                        // The owned sequence is part of the dropped column. Remove an existing one
                        // during action processing so later actions see its relation name as free and
                        // cannot advance it through a later DEFAULT before final ordinal remapping.
                        if let ColumnSource::Original(old_ci) = dropped_source {
                            let owned: Vec<String> = snap
                                .sequences_owned_by(&original.name)
                                .into_iter()
                                .filter(|key| {
                                    snap.sequence(key)
                                        .and_then(|seq| seq.owned_by.as_ref())
                                        .is_some_and(|owner| usize::from(owner.column) == old_ci)
                                })
                                .collect();
                            for key in owned {
                                dropped_owned_sequences.insert(key.clone());
                                self.remove_sequence_routed(&key);
                            }
                        }
                        pending_serials.retain_mut(|seq| {
                            let Some(owner) = &mut seq.owned_by else {
                                return true;
                            };
                            if owner.table != table.name {
                                return true;
                            }
                            if usize::from(owner.column) == ci {
                                return false;
                            }
                            if usize::from(owner.column) > ci {
                                owner.column -= 1;
                            }
                            true
                        });
                        continue;
                    }
                    if let AlterTableEdit::AddPrimaryKey(names) = action {
                        if !table.pk.is_empty() {
                            return Err(EngineError::new(
                                SqlState::InvalidTableDefinition,
                                format!(
                                    "multiple primary keys for table {} are not allowed",
                                    table.name
                                ),
                            ));
                        }
                        let mut pk = Vec::new();
                        for name in names {
                            let Some(ci) = table.columns.iter().position(|c| c.name == name) else {
                                return Err(EngineError::new(
                                    SqlState::UndefinedColumn,
                                    format!("column {name} named in key does not exist"),
                                ));
                            };
                            if pk.contains(&ci) {
                                return Err(EngineError::new(
                                    SqlState::DuplicateColumn,
                                    format!("column {name} appears twice in primary key"),
                                ));
                            }
                            let ty = &table.columns[ci].ty;
                            if !self.type_is_keyable(ty) {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    format!(
                                        "a {} primary key is not supported yet",
                                        ty.canonical_name()
                                    ),
                                ));
                            }
                            pk.push(ci);
                        }
                        for &ci in &pk {
                            table.columns[ci].primary_key = true;
                            table.columns[ci].not_null = true;
                        }
                        table.pk = pk;
                        continue;
                    }
                    if let AlterTableEdit::DropPrimaryKey { cascade } = action {
                        if table.pk.is_empty() {
                            return Err(EngineError::new(
                                SqlState::UndefinedObject,
                                "primary key does not exist".to_string(),
                            ));
                        }
                        let old_pk = sorted_unique(&table.pk);
                        let local_deps = table.foreign_keys.iter().any(|fk| {
                            fk.ref_table == table.name && sorted_unique(&fk.ref_columns) == old_pk
                        });
                        let incoming_deps = snap.tables.iter().any(|(key, child)| {
                            key != &old_key
                                && child.foreign_keys.iter().any(|fk| {
                                    fk.ref_table == original.name
                                        && sorted_unique(&fk.ref_columns) == old_pk
                                })
                        });
                        if !cascade && (local_deps || incoming_deps) {
                            return Err(EngineError::new(
                                SqlState::DependentObjectsStillExist,
                                "cannot drop primary key because other objects depend on it"
                                    .to_string(),
                            )
                            .hinted(error_hint::DROP_CASCADE));
                        }
                        if cascade {
                            table.foreign_keys.retain(|fk| {
                                !(fk.ref_table == table.name
                                    && sorted_unique(&fk.ref_columns) == old_pk)
                            });
                            cascade_unique_cols.push(old_pk);
                        }
                        for &ci in &table.pk {
                            table.columns[ci].primary_key = false;
                        }
                        table.pk.clear();
                        continue;
                    }
                    if let AlterTableEdit::AddConstraint(def) = action {
                        if matches!(
                            &def,
                            AlterConstraintDef::ForeignKey(_) | AlterConstraintDef::Exclude(_)
                        ) {
                            if temp_scope && matches!(&def, AlterConstraintDef::Exclude(_)) {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    "this constraint is not supported on a temporary table",
                                ));
                            }
                            if attachment_scope {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    "this constraint is not supported on an attached-database table",
                                ));
                            }
                        }
                        match def {
                            AlterConstraintDef::Check(d) => {
                                reject_check_structure(&d.expr)?;
                                let (node, ty) = resolve(
                                    &Scope::single(self, &table),
                                    &d.expr,
                                    None,
                                    &mut AggCtx::Forbidden,
                                    &mut ParamTypes::default(),
                                )?;
                                if !matches!(ty, ResolvedType::Bool | ResolvedType::Null) {
                                    return Err(type_error("argument of CHECK must be boolean")
                                        .with_position(d.expr.location().0));
                                }
                                let host_deps =
                                    collect_check_host_deps(&node, self.session.env.functions())?;
                                let name = if let Some(name) = d.name.clone() {
                                    name
                                } else {
                                    let rs = check_referenced_columns(&d.expr, &table.columns);
                                    let column = match rs.as_slice() {
                                        [i] => Some(table.columns[*i].name.to_string()),
                                        _ => None,
                                    };
                                    choose_object_name(
                                        &table.name,
                                        column.as_deref(),
                                        "check",
                                        |n| {
                                            constraint_name_taken(&table, n)
                                                || schema_constraint_name_taken(&snap, &old_key, n)
                                        },
                                    )
                                };
                                if d.name.is_some() && constraint_name_taken(&table, &name) {
                                    return Err(EngineError::new(
                                        SqlState::DuplicateObject,
                                        format!(
                                            "constraint {name} for relation {} already exists",
                                            table.name
                                        ),
                                    ));
                                }
                                table.checks.push(CheckConstraint {
                                    name: name.clone(),
                                    expr_text: d.text,
                                    expr: d.expr,
                                    unparsed: false,
                                    host_deps,
                                });
                                table.checks.sort_by_key(|x| x.name.to_string());
                                added_constraints.insert(name.to_string());
                            }
                            AlterConstraintDef::Unique(d) => {
                                let mut cols = Vec::new();
                                for n in d.columns {
                                    let Some(ci) = table.columns.iter().position(|c| c.name == n)
                                    else {
                                        return Err(EngineError::new(
                                            SqlState::UndefinedColumn,
                                            format!("column {n} named in key does not exist"),
                                        ));
                                    };
                                    if cols.contains(&ci) {
                                        return Err(EngineError::new(
                                            SqlState::DuplicateColumn,
                                            format!(
                                                "column {n} appears twice in unique constraint"
                                            ),
                                        ));
                                    }
                                    cols.push(ci)
                                }
                                for &ci in &cols {
                                    let ty = &table.columns[ci].ty;
                                    if !self.type_is_keyable(ty) {
                                        return Err(EngineError::new(
                                            SqlState::FeatureNotSupported,
                                            format!(
                                                "a {} unique constraint member is not supported yet",
                                                ty.canonical_name()
                                            ),
                                        ));
                                    }
                                }
                                let name = if let Some(name) = d.name {
                                    check_reserved_name("constraint", &name)?;
                                    if working_relation_taken(
                                        &table,
                                        &pending_serials,
                                        &dropped_owned_sequences,
                                        &name,
                                    ) {
                                        return Err(EngineError::new(
                                            SqlState::DuplicateTable,
                                            format!("relation already exists: {name}"),
                                        ));
                                    }
                                    if constraint_name_taken(&table, &name) {
                                        return Err(EngineError::new(
                                            SqlState::DuplicateObject,
                                            format!(
                                                "constraint {name} for relation {} already exists",
                                                table.name
                                            ),
                                        ));
                                    }
                                    name
                                } else {
                                    let names: Vec<&str> =
                                        cols.iter().map(|&i| &*table.columns[i].name).collect();
                                    let addition = name_addition(&names);
                                    choose_object_name(&table.name, Some(&addition), "key", |n| {
                                        working_relation_taken(
                                            &table,
                                            &pending_serials,
                                            &dropped_owned_sequences,
                                            n,
                                        ) || constraint_name_taken(&table, n)
                                            || schema_constraint_name_taken(&snap, &old_key, n)
                                    })
                                };
                                table.indexes.push(IndexDef {
                                    name: name.clone(),
                                    keys: cols.into_iter().map(IndexKey::Column).collect(),
                                    unique: true,
                                    kind: IndexKind::Btree,
                                    predicate: None,
                                    host_deps: Vec::new(),
                                    timezone_deps: TimeZoneDeps::default(),
                                });
                                table.indexes.sort_by_key(|x| x.name.to_string());
                                added_constraints.insert(name.to_string());
                            }
                            AlterConstraintDef::ForeignKey(d) => {
                                let mut local = Vec::new();
                                for n in d.columns {
                                    let Some(ci) = table.columns.iter().position(|c| c.name == n)
                                    else {
                                        return Err(EngineError::new(
                                            SqlState::UndefinedColumn,
                                            format!("column {n} named in key does not exist"),
                                        ));
                                    };
                                    if local.contains(&ci) {
                                        return Err(EngineError::new(
                                            SqlState::DuplicateColumn,
                                            format!(
                                                "column {n} appears twice in foreign key constraint"
                                            ),
                                        ));
                                    }
                                    local.push(ci)
                                }
                                // The parent takes the temp-first resolution walk and must be of
                                // the table's own kind (42P16, temp-tables.md §8).
                                let parent = if d.ref_table == table.name {
                                    table.clone()
                                } else if let Some(p) = temp_namespace.table(&d.ref_table) {
                                    check_fk_persistence(temp_scope, true)?;
                                    p.clone()
                                } else if let Some(p) = main_namespace.table(&d.ref_table) {
                                    check_fk_persistence(temp_scope, false)?;
                                    p.clone()
                                } else {
                                    return Err(fk_parent_missing(&main_namespace, &d.ref_table));
                                };
                                let refs = if let Some(ns) = d.ref_columns {
                                    let mut r = Vec::new();
                                    for n in ns {
                                        let Some(i) =
                                            parent.columns.iter().position(|c| c.name == n)
                                        else {
                                            return Err(EngineError::new(
                                                SqlState::UndefinedColumn,
                                                format!("column {n} named in key does not exist"),
                                            ));
                                        };
                                        if r.contains(&i) {
                                            return Err(EngineError::new(
                                                SqlState::DuplicateColumn,
                                                format!(
                                                    "column {n} appears twice in foreign key constraint"
                                                ),
                                            ));
                                        }
                                        r.push(i)
                                    }
                                    r
                                } else {
                                    if parent.pk.is_empty() {
                                        return Err(EngineError::new(
                                            SqlState::UndefinedObject,
                                            format!(
                                                "there is no primary key for referenced table {}",
                                                parent.name
                                            ),
                                        ));
                                    }
                                    parent.pk.clone()
                                };
                                if local.len() != refs.len() {
                                    return Err(EngineError::new(
                                        SqlState::InvalidForeignKey,
                                        "number of referencing and referenced columns for foreign key disagree",
                                    ));
                                }
                                let rs = sorted_unique(&refs);
                                if !(sorted_unique(&parent.pk) == rs
                                    || parent.indexes.iter().any(|i| {
                                        i.unique
                                            && i.column_ordinals()
                                                .is_some_and(|x| sorted_unique(&x) == rs)
                                    }))
                                {
                                    return Err(EngineError::new(
                                        SqlState::InvalidForeignKey,
                                        format!(
                                            "there is no unique constraint matching given keys for referenced table {}",
                                            parent.name
                                        ),
                                    ));
                                }
                                let name = if let Some(name) = d.name {
                                    if constraint_name_taken(&table, &name) {
                                        return Err(EngineError::new(
                                            SqlState::DuplicateObject,
                                            format!(
                                                "constraint {name} for relation {} already exists",
                                                table.name
                                            ),
                                        ));
                                    }
                                    name
                                } else {
                                    let names: Vec<&str> =
                                        local.iter().map(|&i| &*table.columns[i].name).collect();
                                    let addition = name_addition(&names);
                                    choose_object_name(&table.name, Some(&addition), "fkey", |n| {
                                        constraint_name_taken(&table, n)
                                            || schema_constraint_name_taken(&snap, &old_key, n)
                                    })
                                };
                                for (&li, &ri) in local.iter().zip(&refs) {
                                    if table.columns[li].ty != parent.columns[ri].ty {
                                        return Err(EngineError::new(
                                            SqlState::DatatypeMismatch,
                                            format!(
                                                "foreign key constraint {name} cannot be implemented: key columns {} and {} are of incompatible types: {} and {}",
                                                table.columns[li].name,
                                                parent.columns[ri].name,
                                                table.columns[li].ty.canonical_name(),
                                                parent.columns[ri].ty.canonical_name()
                                            ),
                                        ));
                                    }
                                }
                                table.foreign_keys.push(ForeignKeyConstraint {
                                    name: name.clone(),
                                    columns: local,
                                    ref_table: parent.name,
                                    ref_columns: refs,
                                    match_full: d.match_full,
                                    on_delete: fk_action(d.on_delete, "DELETE")?,
                                    on_update: fk_action(d.on_update, "UPDATE")?,
                                });
                                table.foreign_keys.sort_by_key(|x| x.name.to_string());
                                added_constraints.insert(name.to_string());
                            }
                            AlterConstraintDef::Exclude(d) => {
                                let (cols, els) = resolve_exclusion_definition(
                                    &table,
                                    d.using.as_deref(),
                                    &d.elements,
                                )?;
                                let name = if let Some(name) = d.name {
                                    check_reserved_name("constraint", &name)?;
                                    if working_relation_taken(
                                        &table,
                                        &pending_serials,
                                        &dropped_owned_sequences,
                                        &name,
                                    ) {
                                        return Err(EngineError::new(
                                            SqlState::DuplicateTable,
                                            format!("relation already exists: {name}"),
                                        ));
                                    }
                                    if constraint_name_taken(&table, &name) {
                                        return Err(EngineError::new(
                                            SqlState::DuplicateObject,
                                            format!(
                                                "constraint {name} for relation {} already exists",
                                                table.name
                                            ),
                                        ));
                                    }
                                    name
                                } else {
                                    let names: Vec<&str> =
                                        cols.iter().map(|&i| &*table.columns[i].name).collect();
                                    let addition = name_addition(&names);
                                    choose_object_name(&table.name, Some(&addition), "excl", |n| {
                                        working_relation_taken(
                                            &table,
                                            &pending_serials,
                                            &dropped_owned_sequences,
                                            n,
                                        ) || constraint_name_taken(&table, n)
                                            || schema_constraint_name_taken(&snap, &old_key, n)
                                    })
                                };
                                table.indexes.push(IndexDef {
                                    name: name.clone(),
                                    keys: cols.into_iter().map(IndexKey::Column).collect(),
                                    unique: false,
                                    kind: IndexKind::Gist,
                                    predicate: None,
                                    host_deps: Vec::new(),
                                    timezone_deps: TimeZoneDeps::default(),
                                });
                                table.exclusions.push(ExclusionConstraint {
                                    name: name.clone(),
                                    index: name.clone(),
                                    elements: els,
                                });
                                table.indexes.sort_by_key(|x| x.name.to_string());
                                table.exclusions.sort_by_key(|x| x.name.to_string());
                                added_constraints.insert(name.to_string());
                            }
                        }
                        continue;
                    }
                    if let AlterTableEdit::DropConstraint {
                        name,
                        if_exists,
                        cascade,
                    } = action
                    {
                        let k = name.to_string();
                        let mut found = false;
                        let n = table.checks.len();
                        table.checks.retain(|x| x.name != name);
                        found |= n != table.checks.len();
                        let n = table.foreign_keys.len();
                        table.foreign_keys.retain(|x| x.name != name);
                        found |= n != table.foreign_keys.len();
                        if let Some(e) = table.exclusions.iter().find(|x| x.name == name).cloned() {
                            table.exclusions.retain(|x| x.name != name);
                            table.indexes.retain(|x| x.name != e.index);
                            found = true;
                        }
                        if let Some(ix) = table
                            .indexes
                            .iter()
                            .find(|x| x.unique && x.name == name)
                            .cloned()
                        {
                            let cols = sorted_unique(&ix.column_ordinals().unwrap());
                            let original_cols: Option<Vec<usize>> = cols
                                .iter()
                                .map(|&col| match column_sources.get(col) {
                                    Some(ColumnSource::Original(old)) => Some(*old),
                                    _ => None,
                                })
                                .collect::<Option<Vec<_>>>()
                                .map(|x| sorted_unique(&x));
                            let local_deps = table
                                .foreign_keys
                                .iter()
                                .filter(|f| {
                                    f.ref_table == table.name
                                        && sorted_unique(&f.ref_columns) == cols
                                })
                                .count();
                            let mut external_deps = 0usize;
                            if let Some(old_cols) = &original_cols {
                                for (key, child) in snap.tables.iter() {
                                    if key == &old_key {
                                        continue;
                                    }
                                    for fk in &child.foreign_keys {
                                        let fk_cols = sorted_unique(&fk.ref_columns);
                                        if fk.ref_table == table.name
                                            && fk_cols == *old_cols
                                            && !cascade_dropped_column_fks
                                                .contains(&(key.to_string(), fk.name.to_string()))
                                            && !cascade_unique_cols
                                                .iter()
                                                .any(|prior| *prior == fk_cols)
                                        {
                                            external_deps += 1;
                                        }
                                    }
                                }
                            }
                            let deps = local_deps + external_deps;
                            if deps > 0 && !cascade {
                                return Err(EngineError::new(
                                    SqlState::DependentObjectsStillExist,
                                    format!(
                                        "cannot drop constraint {name} because other objects depend on it"
                                    ),
                                )
                                .hinted(error_hint::DROP_CASCADE));
                            }
                            if cascade {
                                if let Some(old_cols) = original_cols {
                                    cascade_unique_cols.push(old_cols);
                                }
                                table.foreign_keys.retain(|f| {
                                    !(f.ref_table == table.name
                                        && sorted_unique(&f.ref_columns) == cols)
                                });
                            }
                            table.indexes.retain(|x| x.name != name);
                            found = true;
                        }
                        if !found && !if_exists {
                            return Err(EngineError::new(
                                SqlState::UndefinedObject,
                                format!("constraint does not exist: {name}"),
                            ));
                        }
                        added_constraints.remove(&k);
                        continue;
                    }
                    let AlterTableEdit::AlterColumn(edit) = action else {
                        unreachable!()
                    };
                    let Some(ci) = table.columns.iter().position(|c| c.name == edit.column) else {
                        return Err(EngineError::new(
                            SqlState::UndefinedColumn,
                            format!("column does not exist: {}", edit.column),
                        ));
                    };
                    match edit.action {
                        AlterColumnKind::SetDefault(def) => {
                            if table.columns[ci].identity.is_some() {
                                return Err(EngineError::new(
                                    SqlState::SyntaxError,
                                    "identity column cannot have a default".to_string(),
                                ));
                            }
                            let (value, expr) =
                                self.build_alter_default(&table.columns[ci], &def)?;
                            table.columns[ci].default = value;
                            table.columns[ci].default_expr = expr;
                        }
                        AlterColumnKind::DropDefault => {
                            if table.columns[ci].identity.is_some() {
                                return Err(EngineError::new(
                                    SqlState::SyntaxError,
                                    "identity column cannot drop its default".to_string(),
                                ));
                            }
                            table.columns[ci].default = None;
                            table.columns[ci].default_expr = None;
                        }
                        AlterColumnKind::AddIdentity(spec) => {
                            // alter.md §2.8, PostgreSQL's order: the owned sequence is defined
                            // first (type gate 22023, option errors, the name), then the column's
                            // prerequisites (55000).
                            if attachment_scope {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    "a serial / IDENTITY column on an attached-database table is not supported yet",
                                ));
                            }
                            let col = &table.columns[ci];
                            let Some(dtype) = SeqDataType::for_scalar(col.ty.scalar())
                                .filter(|_| col.ty.is_integer())
                            else {
                                return Err(EngineError::new(
                                    SqlState::InvalidParameterValue,
                                    "identity column type must be smallint, integer, or bigint",
                                ));
                            };
                            let mut opts = spec.options.clone();
                            if opts.data_type.is_some() {
                                return Err(EngineError::new(
                                    SqlState::SyntaxError,
                                    "conflicting or redundant options",
                                ));
                            }
                            opts.data_type = Some(dtype.pg_name().to_string());
                            let explicit_name = opts.sequence_name.take();
                            let reserved: Vec<String> =
                                table.indexes.iter().map(|i| i.name.clone()).collect();
                            let seqname = match &explicit_name {
                                Some(n) => n.clone(),
                                None => self.choose_serial_seq_name(
                                    &table.name,
                                    &col.name,
                                    &pending_serials,
                                    &reserved,
                                ),
                            };
                            let seq = build_sequence_def(
                                &seqname,
                                &opts,
                                Some(SeqOwner {
                                    table: table.name.clone(),
                                    column: ci as u16,
                                }),
                            )?;
                            if explicit_name.is_some() {
                                self.check_identity_sequence_name(
                                    &table.name,
                                    &seqname,
                                    &pending_serials,
                                    &reserved,
                                )?;
                            }
                            let col = &table.columns[ci];
                            if !col.not_null {
                                return Err(EngineError::new(
                                    SqlState::ObjectNotInPrerequisiteState,
                                    format!(
                                        "column \"{}\" of relation \"{}\" must be declared NOT NULL before identity can be added",
                                        col.name, table.name
                                    ),
                                ));
                            }
                            if col.identity.is_some() {
                                return Err(EngineError::new(
                                    SqlState::ObjectNotInPrerequisiteState,
                                    format!(
                                        "column \"{}\" of relation \"{}\" is already an identity column",
                                        col.name, table.name
                                    ),
                                ));
                            }
                            if col.default.is_some() || col.default_expr.is_some() {
                                return Err(EngineError::new(
                                    SqlState::ObjectNotInPrerequisiteState,
                                    format!(
                                        "column \"{}\" of relation \"{}\" already has a default value",
                                        col.name, table.name
                                    ),
                                ));
                            }
                            pending_serials.push(seq);
                            let expr_text = nextval_default_text(&seqname);
                            table.columns[ci].default_expr = Some(DefaultExpr {
                                expr: crate::parser::parse_expression(&expr_text)?,
                                expr_text,
                                unparsed: false,
                            });
                            table.columns[ci].identity = Some(if spec.always {
                                IdentityKind::Always
                            } else {
                                IdentityKind::ByDefault
                            });
                        }
                        AlterColumnKind::SetIdentity {
                            generated,
                            options,
                            restart,
                        } => {
                            if table.columns[ci].identity.is_none() {
                                return Err(not_identity_column(&table, ci));
                            }
                            if options != SeqOptions::default() || restart.is_some() {
                                let dtype = SeqDataType::for_scalar(table.columns[ci].ty.scalar())
                                    .expect("identity column is integer");
                                match identity_sequence_slot(
                                    &snap,
                                    &original.name,
                                    &table,
                                    ci,
                                    match column_sources[ci] {
                                        ColumnSource::Original(o) => Some(o),
                                        ColumnSource::Added => None,
                                    },
                                    &pending_serials,
                                    &dropped_owned_sequences,
                                ) {
                                    IdentitySeqSlot::Pending(si) => {
                                        pending_serials[si] = apply_seq_alter(
                                            &pending_serials[si],
                                            &options,
                                            restart,
                                            dtype,
                                        )?;
                                    }
                                    IdentitySeqSlot::Existing(key) => {
                                        let current = self
                                            .sequence(&key)
                                            .cloned()
                                            .expect("identity owned sequence exists");
                                        self.put_sequence_routed(apply_seq_alter(
                                            &current, &options, restart, dtype,
                                        )?);
                                    }
                                }
                            }
                            if let Some(always) = generated {
                                table.columns[ci].identity = Some(if always {
                                    IdentityKind::Always
                                } else {
                                    IdentityKind::ByDefault
                                });
                            }
                        }
                        AlterColumnKind::DropIdentity { if_exists } => {
                            if table.columns[ci].identity.is_none() {
                                if if_exists {
                                    continue;
                                }
                                return Err(not_identity_column(&table, ci));
                            }
                            match identity_sequence_slot(
                                &snap,
                                &original.name,
                                &table,
                                ci,
                                match column_sources[ci] {
                                    ColumnSource::Original(o) => Some(o),
                                    ColumnSource::Added => None,
                                },
                                &pending_serials,
                                &dropped_owned_sequences,
                            ) {
                                IdentitySeqSlot::Pending(si) => {
                                    pending_serials.remove(si);
                                }
                                IdentitySeqSlot::Existing(key) => {
                                    dropped_owned_sequences.insert(key.clone());
                                    self.remove_sequence_routed(&key);
                                }
                            }
                            table.columns[ci].identity = None;
                            table.columns[ci].default = None;
                            table.columns[ci].default_expr = None;
                        }
                        AlterColumnKind::SetNotNull => table.columns[ci].not_null = true,
                        AlterColumnKind::DropNotNull => {
                            if table.columns[ci].identity.is_some() {
                                return Err(EngineError::new(
                                    SqlState::SyntaxError,
                                    "identity column cannot drop not null".to_string(),
                                ));
                            }
                            if table.columns[ci].primary_key {
                                return Err(EngineError::new(
                                    SqlState::InvalidTableDefinition,
                                    "column is in a primary key".to_string(),
                                ));
                            }
                            table.columns[ci].not_null = false;
                        }
                        AlterColumnKind::SetType {
                            type_name,
                            type_mod,
                            using,
                        } => {
                            // A column a view references cannot change type (views.md §6); checked
                            // after every action, so a view a DROP COLUMN … CASCADE drops does not
                            // block it.
                            if let ColumnSource::Original(old_ci) = &column_sources[ci] {
                                view_type_checks.push(*old_ci);
                            }
                            let old_column = table.columns[ci].clone();
                            let mut target = self.build_alter_added_column(
                                &table,
                                &crate::ast::ColumnDef {
                                    name: table.columns[ci].name.clone(),
                                    type_name: type_name.clone(),
                                    type_pos: SrcPos::NONE,
                                    type_mod,
                                    primary_key: false,
                                    not_null: false,
                                    default: None,
                                    identity: None,
                                    collation: None,
                                },
                                &mut Vec::new(),
                                &[],
                                temp_scope,
                                attachment_scope,
                            )?;
                            if old_column.primary_key && !self.type_is_keyable(&target.ty) {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    format!(
                                        "a {} primary key is not supported yet",
                                        target.ty.canonical_name()
                                    ),
                                ));
                            }
                            if table.columns[ci].identity.is_some() && !target.ty.is_integer() {
                                return Err(EngineError::new(
                                    SqlState::InvalidParameterValue,
                                    "identity column type must be smallint, integer, or bigint"
                                        .to_string(),
                                ));
                            }
                            let source = using.unwrap_or_else(|| {
                                Expr::Column(table.columns[ci].name.clone(), SrcPos::NONE)
                            });
                            let cast = Expr::Cast {
                                inner: Box::new(source),
                                type_name: type_name.clone(),
                                type_mod,
                                pos: SrcPos::NONE,
                                cast_pos: SrcPos::NONE,
                                type_pos: SrcPos::NONE,
                            };
                            let (value, ty) = resolve(
                                &Scope::single(self, &table),
                                &cast,
                                None,
                                &mut AggCtx::Forbidden,
                                &mut ParamTypes::default(),
                            )?;
                            if ty != resolved_type_of_col(&target.ty, self) {
                                return Err(type_error(format!(
                                    "USING expression is not of type {}",
                                    target.ty.canonical_name()
                                )));
                            }
                            target.primary_key = table.columns[ci].primary_key;
                            target.not_null = table.columns[ci].not_null;
                            target.identity = table.columns[ci].identity;
                            if let Some(default) = old_column.default.clone() {
                                let default_table = Table {
                                    name: table.name.clone(),
                                    columns: vec![old_column.clone()],
                                    pk: Vec::new(),
                                    checks: Vec::new(),
                                    indexes: Vec::new(),
                                    foreign_keys: Vec::new(),
                                    exclusions: Vec::new(),
                                    on_commit: crate::catalog::OnCommit::PreserveRows,
                                    scalar_func: false,
                                };
                                let default_cast = Expr::Cast {
                                    inner: Box::new(Expr::Column(
                                        old_column.name.clone(),
                                        SrcPos::NONE,
                                    )),
                                    type_name: type_name.clone(),
                                    type_mod,
                                    pos: SrcPos::NONE,
                                    cast_pos: SrcPos::NONE,
                                    type_pos: SrcPos::NONE,
                                };
                                let (node, _) = resolve(
                                    &Scope::single(self, &default_table),
                                    &default_cast,
                                    None,
                                    &mut AggCtx::Forbidden,
                                    &mut ParamTypes::default(),
                                )?;
                                let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
                                let env = EvalEnv {
                                    exec: self,
                                    params: &[],
                                    outer: &[],
                                    rng: &rng,
                                    ctes: CteCtx::empty(),
                                    host_batch: None,
                                };
                                let mut default_meter = self.session.new_meter();
                                target.default =
                                    Some(node.eval(&[default], &env, &mut default_meter)?);
                                prep_cost += default_meter.accrued;
                            } else if let Some(default) = old_column.default_expr.clone() {
                                if default.unparsed {
                                    return Err(unparsable_default_error(
                                        &original.name,
                                        &old_column.name,
                                    ));
                                }
                                let default_cast = Expr::Cast {
                                    inner: Box::new(default.expr),
                                    type_name: type_name.clone(),
                                    type_mod,
                                    pos: SrcPos::NONE,
                                    cast_pos: SrcPos::NONE,
                                    type_pos: SrcPos::NONE,
                                };
                                resolve(
                                    &Scope::empty(self),
                                    &default_cast,
                                    None,
                                    &mut AggCtx::Forbidden,
                                    &mut ParamTypes::default(),
                                )?;
                                let type_sql = match type_mod {
                                    Some(m) => {
                                        let r = m.render();
                                        if r.starts_with(' ') {
                                            format!("{type_name}{r}")
                                        } else {
                                            format!("{type_name} {r}")
                                        }
                                    }
                                    None => type_name.clone(),
                                };
                                target.default_expr = Some(DefaultExpr {
                                    expr_text: format!(
                                        "CAST ( {} AS {} )",
                                        default.expr_text, type_sql
                                    ),
                                    expr: default_cast,
                                    unparsed: false,
                                });
                            }
                            if old_column.identity.is_some() {
                                let old_dtype = SeqDataType::for_scalar(old_column.ty.scalar())
                                    .expect("identity column is integer");
                                let new_dtype = SeqDataType::for_scalar(target.ty.scalar())
                                    .expect("identity column is integer");
                                match identity_sequence_slot(
                                    &snap,
                                    &original.name,
                                    &table,
                                    ci,
                                    match column_sources[ci] {
                                        ColumnSource::Original(o) => Some(o),
                                        ColumnSource::Added => None,
                                    },
                                    &pending_serials,
                                    &dropped_owned_sequences,
                                ) {
                                    IdentitySeqSlot::Pending(si) => {
                                        pending_serials[si] = retype_identity_sequence(
                                            &pending_serials[si],
                                            old_dtype,
                                            new_dtype,
                                        )?;
                                    }
                                    IdentitySeqSlot::Existing(key) => {
                                        let current = self
                                            .sequence(&key)
                                            .cloned()
                                            .expect("identity owned sequence exists");
                                        self.put_sequence_routed(retype_identity_sequence(
                                            &current, old_dtype, new_dtype,
                                        )?);
                                    }
                                }
                            }
                            table.columns[ci] = target;
                            type_changed_columns[ci] = true;
                            type_changed = true;
                            row_steps.push(RowStep::SetType { column: ci, value });
                            for c in &table.checks {
                                added_constraints.insert(c.name.to_string());
                            }
                            for i in &table.indexes {
                                added_constraints.insert(i.name.to_string());
                            }
                            for f in &table.foreign_keys {
                                added_constraints.insert(f.name.to_string());
                            }
                            for e in &table.exclusions {
                                added_constraints.insert(e.name.to_string());
                            }
                        }
                    }
                }
            }
        }
        if let Some(vp) = &view_plan
            && view_type_checks.iter().any(|&o| vp.view_using(o).is_some())
        {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "cannot alter type of a column used by a view",
            ));
        }

        if type_changed {
            for fk in &table.foreign_keys {
                let parent = if fk.ref_table == table.name {
                    &table
                } else {
                    snap.table(&fk.ref_table).expect("existing FK parent")
                };
                for (&local, &referenced) in fk.columns.iter().zip(&fk.ref_columns) {
                    if table.columns[local].ty != parent.columns[referenced].ty {
                        return Err(type_error(format!(
                            "foreign key constraint {} cannot be implemented: key columns {} and {} are of incompatible types: {} and {}",
                            fk.name,
                            table.columns[local].name,
                            parent.columns[referenced].name,
                            table.columns[local].ty.canonical_name(),
                            parent.columns[referenced].ty.canonical_name(),
                        )));
                    }
                }
            }
            for (key, child) in snap.tables.iter() {
                if key == &old_key {
                    continue;
                }
                for fk in &child.foreign_keys {
                    if fk.ref_table != original.name {
                        continue;
                    }
                    if cascade_dropped_column_fks.contains(&(key.to_string(), fk.name.to_string()))
                        || cascade_unique_cols
                            .iter()
                            .any(|cols| *cols == sorted_unique(&fk.ref_columns))
                    {
                        continue;
                    }
                    for (&local, &old_ref) in fk.columns.iter().zip(&fk.ref_columns) {
                        let Some(new_ref) = column_sources.iter().position(
                            |s| matches!(s, ColumnSource::Original(old) if *old == old_ref),
                        ) else {
                            continue;
                        };
                        if child.columns[local].ty != table.columns[new_ref].ty {
                            return Err(type_error(format!(
                                "foreign key constraint {} cannot be implemented: key columns {} and {} are of incompatible types: {} and {}",
                                fk.name,
                                child.columns[local].name,
                                table.columns[new_ref].name,
                                child.columns[local].ty.canonical_name(),
                                table.columns[new_ref].ty.canonical_name(),
                            )));
                        }
                    }
                }
            }
        }

        // Every surviving index on a retyped column must still admit the new type — the CREATE INDEX
        // column gate, checked before any row is rewritten, as PostgreSQL re-creates the index under
        // the new type before its table rewrite (alter.md §3.3).
        for index in &table.indexes {
            for key in &index.keys {
                if let IndexKey::Column(ci) = key
                    && type_changed_columns[*ci]
                {
                    self.check_index_column_type(index.kind, &table.columns[*ci].ty)?;
                }
            }
        }
        let page_size = at
            .db
            .as_deref()
            .filter(|q| !matches!(q.to_string().as_str(), "main" | "temp"))
            .map(|q| self.attach_page_size(q))
            .unwrap_or(self.page_size);
        let rewrite_needed = column_sources.len() != original.columns.len()
            || column_sources
                .iter()
                .enumerate()
                .any(|(i, source)| !matches!(source, ColumnSource::Original(old) if *old == i));
        let pk_rekeyed = table.pk.len() != original.pk.len()
            || table.pk.iter().zip(&original.pk).any(|(&next, &old)| {
                !matches!(column_sources.get(next), Some(ColumnSource::Original(source)) if *source == old)
			})
			|| table
				.pk
				.iter()
				.any(|&column| type_changed_columns[column]);
        let table_rewrite_needed = rewrite_needed || !row_steps.is_empty() || pk_rekeyed;
        let clear_statistics = row_steps
            .iter()
            .any(|step| matches!(step, RowStep::Drop(_) | RowStep::SetType { .. }));
        let rewrite_col_types: Option<Vec<ColType>> = table_rewrite_needed.then(|| {
            let main = self.read_snap();
            table
                .columns
                .iter()
                .map(|c| resolve_col_type(&c.ty, &main.types))
                .collect()
        });
        let mut meter = self.session.new_meter();
        meter.charge(prep_cost);
        meter.guard()?;
        let mut rewrite_entries: Option<Vec<(Vec<u8>, Row)>> = None;
        let mut rewrite_next_rowid = 0i64;
        // Make owned sequences (ADD COLUMN serial/identity, ADD IDENTITY) visible to their
        // synthesized nextval defaults. Statement-level rollback restores the prior working root if
        // any later validation fails.
        for seq in pending_serials.iter().cloned() {
            match at.db.as_deref().map(str::to_string) {
                None if temp_scope => self.temp_working_mut().put_sequence(seq),
                None => self.working_mut().put_sequence(seq),
                Some(q) if q == "temp" => self.temp_working_mut().put_sequence(seq),
                Some(q) if q == "main" => self.working_mut().put_sequence(seq),
                Some(q) => self.attach_write_snap(&q).put_sequence(seq),
            }
        }
        if table_rewrite_needed {
            let old_mask = vec![true; original.columns.len()];
            let store = self.store_scoped(at.db.as_deref(), &original.name);
            let (mut rows, pages, slabs) = store.scan_with_units(&old_mask)?;
            rewrite_next_rowid = store.next_rowid();
            if pk_rekeyed && table.pk.is_empty() {
                rewrite_next_rowid = 0;
            }
            meter.charge(COSTS.page_read * pages as i64 + COSTS.value_decompress * slabs as i64);
            let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
            let env = EvalEnv {
                exec: self,
                params: &[],
                outer: &[],
                rng: &rng,
                ctes: CteCtx::empty(),
                host_batch: None,
            };
            let pk: Vec<(usize, Type)> = table
                .pk
                .iter()
                .map(|&i| (i, table.columns[i].ty.clone()))
                .collect();
            // Resolved column types for the NEW table shape (parallel to `table.columns`) — the
            // vehicle a composite PK key encoder needs when this rewrite re-keys rows (§2.15).
            let col_types: Vec<ColType> = table
                .columns
                .iter()
                .map(|c| self.read_snap().resolve_col_type(&c.ty))
                .collect();
            let colls = self.column_collations(&table.columns);
            let mut seen_keys = std::collections::HashSet::new();
            let mut compress_units = 0i64;
            for (key, row) in &mut rows {
                meter.guard()?;
                meter.charge(COSTS.storage_row_read);
                store.resolve_all(row)?;
                for step in &row_steps {
                    match step {
                        RowStep::Add {
                            column,
                            col_type,
                            default,
                        } => {
                            let value =
                                self.eval_default(column, default.as_ref(), &rng, &mut meter)?;
                            row.push(coerce_for_store(
                                value,
                                col_type,
                                Typmod::of_column(column),
                                column.not_null,
                                &column.name,
                                &self.input_zone(),
                            )?);
                        }
                        RowStep::Drop(column) => {
                            row.remove(*column);
                        }
                        RowStep::SetType { column, value } => {
                            row[*column] = value.eval(row, &env, &mut meter)?;
                        }
                    }
                }
                let next_row = std::mem::take(row);
                for (i, value) in next_row.iter().enumerate() {
                    if table.columns[i].not_null && matches!(value, Value::Null) {
                        return Err(EngineError::not_null_violation(&table.columns[i].name));
                    }
                }
                *row = next_row;
                if pk_rekeyed {
                    if pk.is_empty() {
                        *key = crate::encoding::encode_int(ScalarType::Int64, rewrite_next_rowid);
                        rewrite_next_rowid += 1;
                    } else {
                        *key = encode_pk_key(&pk, &col_types, &colls, row)?;
                    }
                    if !seen_keys.insert(key.clone()) {
                        let pk_ords: Vec<usize> = pk.iter().map(|(o, _)| *o).collect();
                        return Err(EngineError::unique_violation(
                            &table.name,
                            pkey_name(&table.name),
                        )
                        .with_detail(self.pk_detail(
                            &table.name,
                            &table.columns,
                            &pk_ords,
                            row,
                            DUPLICATED,
                        )));
                    }
                }
                compress_units += crate::format::record_compress_units(
                    rewrite_col_types.as_ref().expect("rewrite column types"),
                    key,
                    row,
                    crate::format::page_payload(page_size),
                ) as i64;
            }
            meter.charge(COSTS.value_compress * compress_units);
            meter.guard()?;
            rewrite_entries = Some(rows);
        }

        let mut mask = vec![false; original.columns.len()];
        let mut not_null_names = vec![None::<String>; original.columns.len()];
        for (i, source) in column_sources.iter().enumerate() {
            if let ColumnSource::Original(old) = source {
                if !original.columns[*old].not_null && table.columns[i].not_null {
                    mask[*old] = true;
                    not_null_names[*old] = Some(table.columns[i].name.clone());
                }
            }
        }
        if mask.iter().any(|x| *x) {
            let store = self.store_scoped(at.db.as_deref(), &original.name);
            let (entries, pages, slabs) = store.scan_with_units(&mask)?;
            meter.charge(COSTS.page_read * pages as i64 + COSTS.value_decompress * slabs as i64);
            for (_, mut row) in entries {
                meter.guard()?;
                meter.charge(COSTS.storage_row_read);
                store.resolve_inline_columns(&mut row)?;
                for (i, check) in mask.iter().enumerate() {
                    if *check && matches!(row[i], Value::Null) {
                        return Err(EngineError::not_null_violation(
                            not_null_names[i].as_deref().unwrap(),
                        ));
                    }
                }
            }
        }
        let mut constraint_entries = std::collections::HashMap::<String, Vec<Vec<u8>>>::new();
        if !added_constraints.is_empty() {
            // Presence in this map is also the publication-time identity marker for a newly-added
            // backing index. Seed empty indexes so DROP+ADD name reuse cannot retain the old store.
            for ix in table
                .indexes
                .iter()
                .filter(|ix| added_constraints.contains(&ix.name.to_string()))
            {
                constraint_entries.insert(ix.name.to_string(), Vec::new());
            }
            let store = self.store_scoped(at.db.as_deref(), &original.name);
            let rows = if let Some(rows) = &rewrite_entries {
                rows.clone()
            } else {
                let all_mask = vec![true; table.columns.len()];
                let (rows, pages, slabs) = store.scan_with_units(&all_mask)?;
                meter
                    .charge(COSTS.page_read * pages as i64 + COSTS.value_decompress * slabs as i64);
                rows
            };
            // Only the checks this statement validates (an ADD CONSTRAINT, or every check after ALTER
            // COLUMN TYPE) are resolved, so an existing stale check never blocks an unrelated ADD
            // (spec/design/extensibility.md §7.1.2). Each validated check is evaluated against EVERY
            // row with the current host-function implementations below, so its dependency list is
            // re-collected from this end-state resolution rather than verified: that is the rebuild a
            // DROP + ADD performs, and it lets a type change select a different host overload.
            let mut checks: Vec<(String, RExpr)> = Vec::new();
            for i in 0..table.checks.len() {
                if !added_constraints.contains(&table.checks[i].name.to_string()) {
                    continue;
                }
                if table.checks[i].unparsed {
                    return Err(unparsable_check_error(&table.name, &table.checks[i].name));
                }
                let (node, _) = resolve(
                    &Scope::single(self, &table),
                    &table.checks[i].expr,
                    None,
                    &mut AggCtx::Forbidden,
                    &mut ParamTypes::default(),
                )?;
                table.checks[i].host_deps =
                    collect_check_host_deps(&node, self.session.env.functions())?;
                checks.push((table.checks[i].name.clone(), node));
            }
            let colls = self.column_collations(&table.columns);
            // Resolved column types for the new table shape (composite index key vehicle, §2.15).
            let col_types: Vec<ColType> = table
                .columns
                .iter()
                .map(|c| self.read_snap().resolve_col_type(&c.ty))
                .collect();
            let mut seen =
                std::collections::HashMap::<String, std::collections::HashSet<Vec<u8>>>::new();
            let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
            for (key, mut row) in rows.iter().cloned() {
                meter.guard()?;
                if rewrite_entries.is_none() {
                    meter.charge(COSTS.storage_row_read);
                }
                store.resolve_inline_columns(&mut row)?;
                for ck in &checks {
                    // PostgreSQL's ADD CHECK validation carries no DETAIL (error-fields.md §9.2).
                    self.eval_checks(
                        std::slice::from_ref(ck),
                        &row,
                        &rng,
                        &table.name,
                        &mut meter,
                    )
                    .map_err(|e| e.with_detail(None))?;
                }
                for ix in table
                    .indexes
                    .iter()
                    .filter(|i| added_constraints.contains(&i.name.to_string()))
                {
                    let ri = self.resolve_index(&table, ix)?;
                    if ix.unique {
                        if let Some(p) = self.index_prefix(&col_types, &colls, &ri, &row)? {
                            let s = seen.entry(ix.name.to_string()).or_default();
                            if !s.insert(p) {
                                let env = EvalEnv {
                                    exec: self,
                                    params: &[],
                                    outer: &[],
                                    rng: &rng,
                                    ctes: CteCtx::empty(),
                                    host_batch: None,
                                };
                                return Err(EngineError::unique_violation(&table.name, &ix.name)
                                    .with_detail(self.index_detail(
                                        &table.name,
                                        &table.columns,
                                        ix,
                                        &ri,
                                        &row,
                                        &env,
                                        DUPLICATED,
                                    )));
                            }
                        }
                    }
                    constraint_entries
                        .entry(ix.name.to_string())
                        .or_default()
                        .extend(self.index_entries(
                            &table.columns,
                            &col_types,
                            &colls,
                            &ri,
                            &key,
                            &row,
                        )?);
                }
            }
            for fk in table
                .foreign_keys
                .iter()
                .filter(|f| added_constraints.contains(&f.name.to_string()))
            {
                let parent = if fk.ref_table == table.name {
                    &table
                } else {
                    snap.table(&fk.ref_table).unwrap()
                };
                let pc = self.column_collations(&parent.columns);
                for (_, mut row) in rows.iter().cloned() {
                    store.resolve_inline_columns(&mut row)?;
                    if fk_match_full_violated(fk, &row) {
                        return Err(EngineError::fk_violation_match_full(&table.name, &fk.name));
                    }
                    if let Some(p) = fk_probe(fk, parent, &pc, &row, &fk.columns)? {
                        let hit = if std::ptr::eq(parent, &table) {
                            let mut found = false;
                            for (_, mut candidate) in rows.iter().cloned() {
                                meter.charge(COSTS.constraint_check);
                                meter.guard()?;
                                store.resolve_inline_columns(&mut candidate)?;
                                if let Some(pp) =
                                    fk_probe(fk, parent, &pc, &candidate, &fk.ref_columns)?
                                {
                                    if p.bytes() == pp.bytes() {
                                        found = true;
                                        break;
                                    }
                                }
                            }
                            found
                        } else {
                            self.fk_probe_hits(&p, &parent.name, temp_scope)?
                        };
                        if !hit {
                            return Err(EngineError::fk_violation_insert(&table.name, &fk.name)
                                .with_detail(self.fk_child_detail(
                                    &table.name,
                                    &table.columns,
                                    fk,
                                    &row,
                                )));
                        }
                    }
                }
            }
            for ex in table
                .exclusions
                .iter()
                .filter(|e| added_constraints.contains(&e.name.to_string()))
            {
                for i in 0..rows.len() {
                    let mut a = rows[i].1.clone();
                    store.resolve_inline_columns(&mut a)?;
                    for j in 0..i {
                        meter.charge(COSTS.constraint_check);
                        meter.guard()?;
                        let mut b = rows[j].1.clone();
                        store.resolve_inline_columns(&mut b)?;
                        if exclusion_pair_conflicts(&table.columns, ex, &a, &b)? {
                            return Err(EngineError::exclusion_violation(&table.name, &ex.name)
                                .with_detail(self.exclusion_detail(
                                    &table.name,
                                    &table.columns,
                                    ex,
                                    &a,
                                    &b,
                                    false,
                                )));
                        }
                    }
                }
            }
            for es in constraint_entries.values_mut() {
                es.sort();
            }
        }
        let rekeyed = rewrite_entries.is_some() && pk_rekeyed;
        if rekeyed {
            constraint_entries.clear();
            let colls = self.column_collations(&table.columns);
            let col_types: Vec<ColType> = table
                .columns
                .iter()
                .map(|c| self.read_snap().resolve_col_type(&c.ty))
                .collect();
            for ix in &table.indexes {
                let ri = self.resolve_index(&table, ix)?;
                let out = constraint_entries.entry(ix.name.to_string()).or_default();
                for (key, row) in rewrite_entries.as_ref().unwrap() {
                    out.extend(self.index_entries(
                        &table.columns,
                        &col_types,
                        &colls,
                        &ri,
                        key,
                        row,
                    )?);
                }
                out.sort();
            }
        }
        let ir = index_rename.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
        let ws = match at.db.as_deref().map(str::to_string) {
            None if self.is_temp_table(&original.name) => self.temp_working_mut(),
            None => self.working_mut(),
            Some(q) if q == "temp" => self.temp_working_mut(),
            Some(q) if q == "main" => self.working_mut(),
            Some(q) => self.attach_write_snap(&q),
        };
        if let Some(entries) = &rewrite_entries {
            ws.alter_table_rewrite(
                table.clone(),
                rewrite_col_types.expect("rewrite column types"),
                entries,
                rewrite_next_rowid,
                page_size,
            )?;
        } else {
            ws.alter_table_catalog(&old_key, table.clone(), rename_table, ir);
        }
        if clear_statistics {
            ws.clear_statistics(&table.name);
        }
        if rekeyed {
            ws.rebuild_alter_indexes(&original, &table, &constraint_entries, page_size)?;
        } else {
            ws.sync_alter_constraint_indexes(&original, &table, &constraint_entries, page_size)?;
        }
        ws.cascade_dropped_unique_fks(&table.name, &cascade_unique_cols);
        if rewrite_needed {
            let mut original_to_new = vec![None; original.columns.len()];
            for (next, source) in column_sources.iter().enumerate() {
                if let ColumnSource::Original(old) = source {
                    original_to_new[*old] = Some(next);
                }
            }
            let pending: std::collections::HashSet<String> =
                pending_serials.iter().map(|s| s.name.to_string()).collect();
            ws.remap_alter_column_dependents(&table.name, &original_to_new, &pending);
        }
        // Drop the views a DROP COLUMN … CASCADE took (views.md §6), then rewrite and re-validate
        // the surviving dependent views against the post-statement catalog (§7/§8); a failure is
        // 0A000 and discards the whole statement.
        for name in &cascade_dropped_views {
            self.working_mut().remove_view(name);
        }
        if let Some(plan) = view_plan {
            let change = view_change.unwrap_or_else(|| ViewAlterChange::Other {
                dropped: (0..original.columns.len())
                    .filter(|old| {
                        !column_sources
                            .iter()
                            .any(|s| matches!(s, ColumnSource::Original(o) if o == old))
                    })
                    .collect(),
            });
            self.finish_view_alter(plan, &change)?;
        }
        Ok(Outcome::Statement {
            cost: meter.accrued,
            rows_affected: None,
        })
    }

    fn rewrite_table_expressions(
        &self,
        table: &mut Table,
        column: Option<(&str, &str)>,
        rename: Option<&str>,
    ) -> Result<()> {
        // A rename rewrites every stored expression's text, which needs its tokens to parse
        // (spec/design/unparsable-definitions.md §3.2–§3.4).
        table.unparsed_error(true)?;
        let table_name = table.name.clone();
        let rewrite = |text: &str| match column {
            Some((old, new)) => {
                crate::parser::rewrite_column_identifier(text, &table_name, old, new)
            }
            None => crate::parser::rewrite_table_qualifier(text, &table_name, rename.unwrap()),
        };
        for c in &mut table.checks {
            let (text, expr) = rewrite(&c.expr_text)?;
            c.expr_text = text;
            c.expr = expr;
        }
        for c in &mut table.columns {
            if let Some(d) = &mut c.default_expr {
                let (text, expr) = rewrite(&d.expr_text)?;
                d.expr_text = text;
                d.expr = expr;
            }
        }
        for i in &mut table.indexes {
            for k in &mut i.keys {
                if let IndexKey::Expr(e) = k {
                    let (text, expr) = rewrite(&e.expr_text)?;
                    e.expr_text = text;
                    e.expr = expr;
                }
            }
            if let Some(p) = &mut i.predicate {
                let (text, expr) = rewrite(&p.expr_text)?;
                p.expr_text = text;
                p.expr = expr;
            }
        }
        Ok(())
    }

    fn build_alter_default(
        &self,
        col: &Column,
        def: &DefaultDef,
    ) -> Result<(Option<Value>, Option<DefaultExpr>)> {
        let Type::Scalar(sty) = &col.ty else {
            // A bare `DEFAULT NULL` on a container column is the same as no default (PostgreSQL
            // stores none for it); any other container DEFAULT is not supported yet.
            if is_bare_null_default(def) {
                return Ok((None, None));
            }
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a DEFAULT on this container type is not supported yet".to_string(),
            ));
        };
        let sty = *sty;
        let clock_default = sty.is_date()
            && matches!(&def.expr, Expr::Literal(Literal::Text(s), _) if crate::date::date_clock_is_relative(s));
        if let Expr::Literal(lit, _) = &def.expr
            && !clock_default
        {
            return Ok((
                Some(
                    store_value(
                        literal_to_value_for(lit, sty)?,
                        sty,
                        Typmod::of_column(col),
                        false,
                        &col.name,
                        &self.input_zone(),
                    )
                    .map_err(assignment_hint)?,
                ),
                None,
            ));
        }
        reject_default_structure(&def.expr)?;
        let scope = Scope::empty(self);
        let (_, rt) = resolve(
            &scope,
            &def.expr,
            Some(sty),
            &mut AggCtx::Forbidden,
            &mut ParamTypes::default(),
        )?;
        if !rt.assignable_to(sty) {
            return Err(assignment_error(format!(
                "column {} is of type {} but default expression is of type {}",
                col.name,
                sty.canonical_name(),
                rt.type_name()
            )));
        }
        Ok((
            None,
            Some(DefaultExpr {
                expr_text: def.text.clone(),
                expr: def.expr.clone(),
                unparsed: false,
            }),
        ))
    }

    /// Resolve one ADD COLUMN definition using the same type/default/identity rules as CREATE
    /// TABLE. Inline CHECK/UNIQUE/REFERENCES definitions are deliberately handled by the existing
    /// ALTER ADD CONSTRAINT path after this returns.
    fn build_alter_added_column(
        &self,
        table: &Table,
        def: &crate::ast::ColumnDef,
        pending_serials: &mut Vec<SequenceDef>,
        reserved_relation_names: &[String],
        temp_scope: bool,
        attachment_scope: bool,
    ) -> Result<Column> {
        let serial_kind = serial_pseudo_type(&def.type_name);
        let mut temporal: Option<TemporalTypmod> = None;
        let (ty, decimal, varchar_len): (Type, Option<DecimalTypmod>, Option<u32>) =
            if let Some(sk) = serial_kind {
                if def.type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!("type modifier is not allowed for type {}", def.type_name),
                    ));
                }
                (Type::Scalar(sk), None, None)
            } else if let Some(base) = def.type_name.strip_suffix("[]") {
                if def.type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier on an array type is not supported yet",
                    ));
                }
                match ScalarType::from_name(base) {
                    Some(s) => (Type::Array(Box::new(Type::Scalar(s))), None, None),
                    None => match self.read_snap().composite_type(base) {
                        Some(c) => (
                            Type::Array(Box::new(Type::Composite(crate::types::CompositeRef {
                                name: c.name.clone(),
                            }))),
                            None,
                            None,
                        ),
                        None => {
                            return Err(EngineError::new(
                                SqlState::UndefinedObject,
                                format!("type does not exist: {base}"),
                            )
                            .with_position(def.type_pos.0));
                        }
                    },
                }
            } else if let Some(rdesc) = crate::range::range_by_name(&def.type_name) {
                if def.type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier on a range type is not supported",
                    ));
                }
                (
                    Type::Range(Box::new(Type::Scalar(crate::range::element_scalar(rdesc)))),
                    None,
                    None,
                )
            } else if ScalarType::from_name(&def.type_name).is_some() {
                let (s, tm) = resolve_type_and_typmod(&def.type_name, &def.type_mod)?;
                if s == ScalarType::JsonPath {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a jsonpath column is not supported yet",
                    ));
                }
                temporal = tm.temporal;
                (Type::Scalar(s), tm.decimal, tm.varchar_len)
            } else if let Some(c) = self.read_snap().composite_type(&def.type_name) {
                if def.type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "a type modifier is not supported for composite type {}",
                            def.type_name
                        ),
                    ));
                }
                (
                    Type::Composite(crate::types::CompositeRef {
                        name: c.name.clone(),
                    }),
                    None,
                    None,
                )
            } else {
                return Err(EngineError::new(
                    SqlState::UndefinedObject,
                    format!("type does not exist: {}", def.type_name),
                )
                .with_position(def.type_pos.0));
            };

        let collation = if let Some(name) = &def.collation {
            if !ty.is_collatable() {
                return Err(type_error(format!(
                    "collations are not supported by type {}",
                    ty.canonical_name()
                )));
            }
            resolve_collation_name(self, name)?;
            if name == "C" {
                None
            } else {
                Some(name.clone())
            }
        } else if ty.is_collatable() {
            self.read_snap().default_collation().map(str::to_string)
        } else {
            None
        };
        if temp_scope && collation.is_some() {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "COLLATE on a temporary-table column is not yet supported",
            ));
        }
        if attachment_scope && (ty.is_composite() || collation.is_some()) {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "this column type or collation is not supported on an attached-database table",
            ));
        }

        let mut col = Column {
            name: def.name.clone(),
            ty,
            decimal,
            varchar_len,
            temporal,
            primary_key: def.primary_key,
            not_null: def.primary_key
                || def.not_null
                || serial_kind.is_some()
                || def.identity.is_some(),
            default: None,
            default_expr: None,
            identity: None,
            collation,
            no_collation: false,
            collation_conflict: false,
        };
        if def.primary_key {
            if !table.pk.is_empty() {
                return Err(EngineError::new(
                    SqlState::InvalidTableDefinition,
                    format!(
                        "multiple primary keys for table {} are not allowed",
                        table.name
                    ),
                ));
            }
            if !self.type_is_keyable(&col.ty) {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "a {} primary key is not supported yet",
                        col.ty.canonical_name()
                    ),
                ));
            }
        }
        if serial_kind.is_some() || def.identity.is_some() {
            if attachment_scope {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a serial / IDENTITY column on an attached-database table is not supported yet",
                ));
            }
            if def.identity.is_some() && !col.ty.is_integer() {
                return Err(EngineError::new(
                    SqlState::InvalidParameterValue,
                    "identity column type must be smallint, integer, or bigint",
                ));
            }
            if def.identity.is_some() && (def.default.is_some() || serial_kind.is_some()) {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    format!(
                        "both default and identity specified for column {} of table {}",
                        def.name, table.name
                    ),
                ));
            }
            if serial_kind.is_some() && def.default.is_some() {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    format!(
                        "multiple default values specified for column {} of table {}",
                        def.name, table.name
                    ),
                ));
            }
            let mut opts = def
                .identity
                .as_ref()
                .map(|x| x.options.clone())
                .unwrap_or_default();
            let seqname = match opts.sequence_name.take() {
                Some(n) => {
                    self.check_identity_sequence_name(
                        &table.name,
                        &n,
                        pending_serials,
                        reserved_relation_names,
                    )?;
                    n
                }
                None => self.choose_serial_seq_name(
                    &table.name,
                    &def.name,
                    pending_serials,
                    reserved_relation_names,
                ),
            };
            if opts.data_type.is_some() {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "conflicting or redundant options",
                ));
            }
            let seq_scalar = serial_kind.unwrap_or_else(|| col.ty.scalar());
            opts.data_type = Some(
                SeqDataType::for_scalar(seq_scalar)
                    .expect("serial/identity is integer")
                    .pg_name()
                    .to_string(),
            );
            pending_serials.push(build_sequence_def(
                &seqname,
                &opts,
                Some(SeqOwner {
                    table: table.name.clone(),
                    column: table.columns.len() as u16,
                }),
            )?);
            let expr_text = nextval_default_text(&seqname);
            col.default_expr = Some(DefaultExpr {
                expr: crate::parser::parse_expression(&expr_text)?,
                expr_text,
                unparsed: false,
            });
            col.identity = def.identity.as_ref().map(|id| {
                if id.always {
                    IdentityKind::Always
                } else {
                    IdentityKind::ByDefault
                }
            });
        } else if col.ty.is_composite() || col.ty.is_array() || col.ty.is_range() {
            // A bare `DEFAULT NULL` is the same as no default (see `create_table`).
            if def
                .default
                .as_ref()
                .is_some_and(|d| !is_bare_null_default(d))
            {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a DEFAULT on a composite-, array-, or range-typed column is not supported yet",
                ));
            }
        } else if let Some(default) = &def.default {
            let (value, expr) = self.build_alter_default(&col, default)?;
            col.default = value;
            col.default_expr = expr;
        }
        Ok(col)
    }

    /// Resolve a table's CHECK constraints for a write statement: each stored expression
    /// against a one-relation scope, in the catalog's (evaluation/name) order. Cannot fail
    /// for a catalog produced by CREATE TABLE or a well-formed file (both validated); a
    /// hand-corrupted expression surfaces its natural resolve error.
    pub(crate) fn resolve_checks(&self, table: &Table) -> Result<Vec<(String, RExpr)>> {
        self.resolve_checks_with_params(table, &mut ParamTypes::default())
    }

    pub(crate) fn resolve_checks_with_params(
        &self,
        table: &Table,
        params: &mut ParamTypes,
    ) -> Result<Vec<(String, RExpr)>> {
        if table.checks.is_empty() {
            return Ok(Vec::new());
        }
        let scope = Scope::single(self, table);
        let mut out = Vec::with_capacity(table.checks.len());
        for c in &table.checks {
            if c.unparsed {
                return Err(unparsable_check_error(&table.name, &c.name));
            }
            let (node, _) = resolve(&scope, &c.expr, None, &mut AggCtx::Forbidden, params)?;
            // Reopen soundness (spec/design/extensibility.md §7.1.2): every host call must still match
            // the dependency persisted with the check, else XX002 before any mutation.
            verify_check_host_deps(&c.name, &node, self.session.env.functions(), &c.host_deps)?;
            out.push((c.name.clone(), node));
        }
        Ok(out)
    }

    /// Resolve an index's key elements for one statement's maintenance (spec/design/indexes.md §4),
    /// modeled on [`resolve_checks`](Self::resolve_checks): a column key keeps its ordinal; an
    /// expression key resolves against the table's columns to an `RExpr` + its encoding `Type` +
    /// collation. The expression was validated (immutable, indexable result) at CREATE INDEX, so
    /// resolution here cannot newly fail (an aggregate/window/subquery/param was rejected then, and
    /// re-resolving with `AggCtx::Forbidden` is inert). Returns an owned [`ResolvedIndex`], so the
    /// write paths can hold it while mutating stores.
    pub(crate) fn resolve_index(&self, table: &Table, def: &IndexDef) -> Result<ResolvedIndex> {
        self.resolve_index_with_params(table, def, &mut ParamTypes::default())
    }

    pub(crate) fn resolve_index_with_params(
        &self,
        table: &Table,
        def: &IndexDef,
        params: &mut ParamTypes,
    ) -> Result<ResolvedIndex> {
        // An index whose stored expression did not parse can neither be maintained nor used: the read
        // path swallows this error and skips the index, a write propagates it
        // (spec/design/unparsable-definitions.md §3.4).
        if def.unparsed() {
            return Err(unparsable_index_error(&table.name, &def.name));
        }
        params.uncacheable |= def.timezone_deps.dynamic;
        verify_index_timezone_deps(&def.timezone_deps, &self.session.env)?;
        let mut keys = Vec::with_capacity(def.keys.len());
        for k in &def.keys {
            match k {
                IndexKey::Column(ord) => keys.push(ResolvedKey::Column(*ord)),
                IndexKey::Expr(e) => {
                    let scope = Scope::index(self, table);
                    let (rexpr, rtype) =
                        resolve(&scope, &e.expr, None, &mut AggCtx::Forbidden, params)?;
                    let ty = resolved_to_key_type(&rtype)
                        .expect("index expression result type validated indexable at CREATE INDEX");
                    let coll = resolve_deriv(scope.catalog, derive_collation(&scope, &e.expr)?)?;
                    keys.push(ResolvedKey::Expr(rexpr, ty, coll));
                }
            }
        }
        // A partial index's predicate (indexes.md §9), re-resolved against the table's columns — it
        // was validated boolean + immutable at CREATE INDEX, so this cannot newly fail (a
        // `Forbidden` re-resolve of an aggregate/window/param/subquery-free boolean is inert).
        let predicate = match &def.predicate {
            None => None,
            Some(p) => {
                let scope = Scope::index(self, table);
                Some(resolve_boolean_filter(&scope, &p.expr, params)?)
            }
        };
        // Reopen soundness (extensibility.md §8.1/§11, step 4): if this index depends on host
        // functions, verify each still matches the dependency persisted with the index. A mismatch is
        // `XX002`, which the read path swallows (`resolve_index(...).ok()?` → the planner skips it and
        // heap-scans) and a write propagates (the table is read-only until a rebuild). A no-op for the
        // common index that records no host dependency.
        if !def.host_deps.is_empty() {
            let reg = self.session.env.functions();
            for k in &keys {
                if let ResolvedKey::Expr(rexpr, ..) = k {
                    verify_index_host_deps(rexpr, reg, &def.host_deps)?;
                }
            }
            if let Some(pred) = &predicate {
                verify_index_host_deps(pred, reg, &def.host_deps)?;
            }
        }
        Ok(ResolvedIndex {
            name: def.name.clone(),
            unique: def.unique,
            kind: def.kind,
            keys,
            predicate,
        })
    }

    /// Resolve every index of a table once per statement (the maintenance driver — INSERT / UPDATE
    /// / DELETE build their `ResolvedIndex` list up front, parallel to `table.indexes`).
    pub(crate) fn resolve_table_indexes(&self, table: &Table) -> Result<Vec<ResolvedIndex>> {
        self.resolve_table_indexes_with_params(table, &mut ParamTypes::default())
    }

    pub(crate) fn resolve_table_indexes_with_params(
        &self,
        table: &Table,
        params: &mut ParamTypes,
    ) -> Result<Vec<ResolvedIndex>> {
        table
            .indexes
            .iter()
            .map(|d| self.resolve_index_with_params(table, d, params))
            .collect()
    }

    /// A row's secondary-index entry keys for maintenance (spec/design/indexes.md §4), building the
    /// unmetered eval env internally (an index expression is immutable — no params/CTE/seam, so the
    /// fresh statement rng is never read). Returns owned bytes, so callers compute all entries
    /// through this `&self` call BEFORE taking a `&mut` store borrow to write them.
    pub(crate) fn index_entries(
        &self,
        columns: &[Column],
        col_types: &[ColType],
        colls: &[Option<std::sync::Arc<Collation>>],
        rindex: &ResolvedIndex,
        storage_key: &[u8],
        row: &Row,
    ) -> Result<Vec<Vec<u8>>> {
        let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params: &[],
            outer: &[],
            rng: &rng,
            ctes: CteCtx::empty(),
            host_batch: None,
        };
        index_entry_keys(columns, col_types, colls, rindex, storage_key, row, &env)
    }

    /// A row's uniqueness-probe prefix for one index (spec/design/indexes.md §8), building the
    /// unmetered eval env internally (as [`index_entries`](Self::index_entries)). `col_types` is the
    /// table's resolved column types (composite index columns key through their field codec, §2.15).
    pub(crate) fn index_prefix(
        &self,
        col_types: &[ColType],
        colls: &[Option<std::sync::Arc<Collation>>],
        rindex: &ResolvedIndex,
        row: &Row,
    ) -> Result<Option<Vec<u8>>> {
        let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params: &[],
            outer: &[],
            rng: &rng,
            ctes: CteCtx::empty(),
            host_batch: None,
        };
        index_prefix_key(col_types, colls, rindex, row, &env)
    }

    /// A candidate row's arbiter key for `ON CONFLICT` (spec/design/upsert.md §3), building the
    /// unmetered eval env internally (an expression-index arbiter evaluates its keys — as
    /// [`index_prefix`](Self::index_prefix)).
    pub(crate) fn arbiter_probe_key(
        &self,
        arb: &Arbiter,
        pk: &[(usize, Type)],
        col_types: &[ColType],
        colls: &[Option<std::sync::Arc<Collation>>],
        rindexes: &[ResolvedIndex],
        row: &Row,
    ) -> Result<Option<Vec<u8>>> {
        let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params: &[],
            outer: &[],
            rng: &rng,
            ctes: CteCtx::empty(),
            host_batch: None,
        };
        arbiter_key(arb, pk, col_types, colls, rindexes, row, &env)
    }

    /// Resolve each column's **expression** `DEFAULT` (constraints.md §2) to an `RExpr`, once
    /// per INSERT statement — `insert_rows` (and the VALUES `DEFAULT`-keyword materialization)
    /// evaluate it per omitted/`DEFAULT` slot. Returns a slot per column (parallel to
    /// `table.columns`): `Some(node)` for an expression default, `None` for a column with a
    /// constant default or no default. The default resolves against an EMPTY scope (no columns;
    /// a column reference was rejected 0A000 at CREATE TABLE) with the column's type as the
    /// adaptable-operand hint.
    pub(crate) fn resolve_default_exprs(&self, table: &Table) -> Result<Vec<Option<RExpr>>> {
        self.resolve_default_exprs_with_params(table, &mut ParamTypes::default())
    }

    pub(crate) fn resolve_default_exprs_with_params(
        &self,
        table: &Table,
        params: &mut ParamTypes,
    ) -> Result<Vec<Option<RExpr>>> {
        self.resolve_default_exprs_for(table, None, params)
    }

    /// [`resolve_default_exprs_with_params`](Self::resolve_default_exprs_with_params) restricted to
    /// the columns `need` marks (`None` = every column): a statement resolves only the defaults it can
    /// apply, so a host function behind a default it never applies need not be registered
    /// (spec/design/extensibility.md §7.1.1). An unneeded column's slot stays `None`.
    pub(crate) fn resolve_default_exprs_for(
        &self,
        table: &Table,
        need: Option<&[bool]>,
        params: &mut ParamTypes,
    ) -> Result<Vec<Option<RExpr>>> {
        let mut out = Vec::with_capacity(table.columns.len());
        for (i, col) in table.columns.iter().enumerate() {
            if need.is_some_and(|n| !n[i]) {
                out.push(None);
                continue;
            }
            match &col.default_expr {
                Some(de) if de.unparsed => {
                    return Err(unparsable_default_error(&table.name, &col.name));
                }
                Some(de) => {
                    let scope = Scope::empty(self);
                    let (node, _) = resolve(
                        &scope,
                        &de.expr,
                        Some(col.ty.scalar()),
                        &mut AggCtx::Forbidden,
                        params,
                    )?;
                    out.push(Some(node));
                }
                None => out.push(None),
            }
        }
        Ok(out)
    }

    /// The value an omitted column or a `DEFAULT` value slot takes (constraints.md §2): the
    /// column's pre-evaluated constant (`col.default`, or NULL when it has none), OR — for an
    /// expression default — the resolved `RExpr` evaluated against an empty row through the
    /// per-statement seam/clock (`rng`) and metered (`operator_eval` per node). Reused by the
    /// VALUES materialization (a `DEFAULT` keyword) and `insert_rows` (an omitted column),
    /// sharing ONE `StmtRng` so a multi-row `DEFAULT uuidv7()` stays monotonic.
    pub(crate) fn eval_default(
        &self,
        col: &Column,
        default_rexpr: Option<&RExpr>,
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        meter: &mut Meter,
    ) -> Result<Value> {
        match default_rexpr {
            Some(rx) => {
                meter.guard()?;
                let env = EvalEnv {
                    exec: self,
                    params: &[],
                    outer: &[],
                    rng,
                    ctes: CteCtx::empty(),
                    host_batch: None,
                };
                rx.eval(&[], &env, meter)
            }
            None => Ok(col.default.clone().unwrap_or(Value::Null)),
        }
    }

    /// Run a `DROP TABLE [IF EXISTS] a [, …] [CASCADE | RESTRICT]`: remove each named table's
    /// definition and row store from the catalog (keyed by exact name). Two-phase /
    /// all-or-nothing (spec/design/grammar.md §13): every name is resolved and validated first
    /// — a missing table is 42P01 (unless `IF EXISTS` skips just that name), a non-table relation
    /// is 42809, and an external FK dependent is 2BP01 under `RESTRICT` — and only if the whole
    /// list checks out is anything removed. A repeated name is deduplicated; a FK between two
    /// tables both in the drop set never blocks; `CASCADE` drops the surviving tables' now-dangling
    /// FK constraints. Like CREATE TABLE it touches no rows and evaluates no expression tree, so it
    /// accrues zero cost.
    pub(crate) fn execute_drop_table(&mut self, dt: DropTable) -> Result<Outcome> {
        // The scope a resolved target lives in (temp-tables.md §3) — it governs which working
        // snapshot the removal routes to in phase 3.
        enum Scope {
            Temp,
            Persistent,
        }
        // ---- Phase 1: resolve & classify every name into the drop set. Nothing is removed yet.
        // A repeated name is deduplicated (PG collects the targets into a set, so `DROP TABLE a, a`
        // drops `a` once and succeeds); `seen` is the set of exact keys actually being dropped.
        let mut targets: Vec<(String, Scope)> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for name in &dt.names {
            let key = name.to_string();
            if seen.contains(&key) {
                continue; // already resolved this exact target (deduplicated)
            }
            // A built-in catalog relation resolves BEFORE the user catalog (introspection.md §5),
            // and a system relation cannot be dropped: 42809. IF EXISTS does not suppress this (the
            // relation exists — this is a kind rejection, not a missing name).
            if is_catalog_rel_name(&key) {
                return Err(EngineError::new(
                    SqlState::WrongObjectType,
                    format!("cannot drop system relation \"{key}\""),
                ));
            }
            // Resolution walk: session-local temp → persistent. Preclude-overlaps keeps a name in at
            // most one scope, so this is just "where it lives" (temp-tables.md §3).
            let scope = if self.is_temp_table(name) {
                Scope::Temp
            } else if self.read_snap().table(name).is_some() {
                Scope::Persistent
            } else {
                // Not a table in any scope. An index's name is the wrong object kind (42809 —
                // indexes.md §2); `IF EXISTS` does NOT suppress this. Otherwise a missing table is
                // 42P01, unless `IF EXISTS` makes it a no-op for just this name (PG turns the
                // missing-table error into a notice).
                if self.find_index(name).is_some() || self.is_view(name) {
                    return Err(EngineError::new(
                        SqlState::WrongObjectType,
                        format!("{name} is not a table"),
                    ));
                }
                if dt.if_exists {
                    continue;
                }
                return Err(EngineError::new(
                    SqlState::UndefinedTable,
                    format!("table does not exist: {name}"),
                ));
            };
            seen.insert(key.clone());
            targets.push((key, scope));
        }
        // ---- Phase 2: FK dependency check (RESTRICT) / removal collection (CASCADE). An FK never
        // crosses domains (temp-tables.md §8), so each domain's drop set is scanned in its own
        // snapshot — the main database first, then the session-local temp domain; a dependent whose
        // referencing table is itself in the drop set does not count (so `DROP TABLE parent, child`
        // succeeds even under RESTRICT).
        // A view depending on a table being dropped blocks the statement under RESTRICT; CASCADE
        // drops it, with every view depending on it (views.md §6).
        let persistent: Vec<String> = targets
            .iter()
            .filter(|(_, scope)| matches!(scope, Scope::Persistent))
            .map(|(key, _)| key.clone())
            .collect();
        let cascade_views = self.drop_table_views(&persistent, dt.cascade)?;
        let temp_seen: BTreeSet<String> = targets
            .iter()
            .filter(|(_, scope)| matches!(scope, Scope::Temp))
            .map(|(key, _)| key.clone())
            .collect();
        let persistent_seen: BTreeSet<String> = persistent.iter().cloned().collect();
        let main_deps = self
            .read_snap()
            .foreign_key_dependents_excluding(&persistent_seen);
        let temp_deps = self
            .temp_read_snap()
            .foreign_key_dependents_excluding(&temp_seen);
        let (cascade_removals, temp_cascade_removals) = if dt.cascade {
            (main_deps, temp_deps)
        } else {
            let deps: Vec<&FkDependent> = main_deps.iter().chain(&temp_deps).collect();
            // RESTRICT (the default, and the bare form's behavior): an external FK dependent blocks
            // the drop with 2BP01 — the same message the single-table check produced.
            if let Some(d) = deps.first() {
                return Err(EngineError::new(
                    SqlState::DependentObjectsStillExist,
                    format!(
                        "cannot drop table {} because other objects depend on it: constraint {} on table {}",
                        d.dropped_name, d.fk_name, d.ref_table_name
                    ),
                )
                .hinted(error_hint::DROP_CASCADE));
            }
            (Vec::new(), Vec::new())
        };
        // ---- Phase 3: apply. CASCADE first drops the dependent views and each surviving table's
        // now-dangling FK constraint (in place, preserving its rows), each in its own domain: views
        // live only in the main database, and an FK in its tables' domain.
        for name in &cascade_views {
            self.working_mut().remove_view(name);
        }
        for d in &cascade_removals {
            self.working_mut()
                .remove_foreign_key(&d.ref_table_key, &d.fk_name);
        }
        for d in &temp_cascade_removals {
            self.temp_working_mut()
                .remove_foreign_key(&d.ref_table_key, &d.fk_name);
        }
        // Then remove every target from its own scope, auto-dropping the sequences it owns — a
        // `serial`/IDENTITY column's owned sequence (spec/design/sequences.md §12; an owned sequence
        // is never an FK dependent, so the phase-2 check never blocked on it). A temp drop touches
        // only its temp snapshot, never the main image, so it makes zero file writes.
        for (key, scope) in &targets {
            match scope {
                Scope::Temp => {
                    let owned = self.temp_read_snap().sequences_owned_by(key);
                    let w = self.temp_working_mut();
                    for sk in &owned {
                        w.remove_sequence(sk);
                    }
                    w.remove_table(key);
                }
                Scope::Persistent => {
                    let owned = self.read_snap().sequences_owned_by(key);
                    let w = self.working_mut();
                    for sk in &owned {
                        w.remove_sequence(sk);
                    }
                    w.remove_table(key);
                }
            }
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a CREATE INDEX (spec/design/indexes.md §2). Validation mirrors
    /// PostgreSQL's order (oracle-probed): the table must exist (42P01); each key column, in
    /// list order, must exist (42703) and be of a key-encodable type (0A000 — the same
    /// narrowing as a PRIMARY KEY member); then an explicit name is checked against the
    /// shared relation namespace (42P07), or an omitted name derives PG's choice — the
    /// exact `<table>_<col>..._idx` with the smallest free suffix. The index is then
    /// built by scanning the table once: `page_read` per node + `storage_row_read` per row
    /// (the metered build scan — cost.md §3); maintenance thereafter is unmetered.
    pub(crate) fn execute_create_index(&mut self, ci: CreateIndex) -> Result<Outcome> {
        // A standalone CREATE INDEX targets whichever scope owns the table — session-local temp,
        // persistent, or a host-attached database (temp-tables.md §8, attached-databases.md §3). The
        // build below is scope-agnostic (the scoped `table`/`store`/`index_store_mut` funnels route by
        // the qualifier + resolution walk); only the catalog `put_index` write must target the owning
        // snapshot, so the routing happens there.
        // A built-in catalog relation cannot be indexed (introspection.md §5): 42809, checked by
        // NAME before qualifier validation, like the DML targets.
        check_catalog_rel_write(&ci.table)?;
        // A DDL write to a READ-ONLY host attachment is 25006 before any work — checked BEFORE the
        // qualifier existence gate so a read-only attachment refuses the write deterministically (§4).
        self.check_attachment_writable(ci.db.as_deref())?;
        // An index on a view is the wrong object kind (views.md §2).
        if matches!(ci.db.as_deref(), None | Some("main"))
            && self.table_scoped(ci.db.as_deref(), &ci.table).is_none()
            && self.is_view(&ci.table)
        {
            return Err(EngineError::new(
                SqlState::WrongObjectType,
                format!("cannot create index on relation \"{}\"", ci.table),
            ));
        }
        self.check_table_qualifier(ci.db.as_deref(), &ci.table)?; // attached-databases.md §3
        let attach_name: Option<String> = if is_attachment_scope(ci.db.as_deref()) {
            ci.db.as_ref().map(|d| d.to_string())
        } else {
            None
        };
        let table = self
            .table_scoped(ci.db.as_deref(), &ci.table)
            .ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedTable,
                    format!("table does not exist: {}", ci.table),
                )
            })?;
        let table_key = table.name.to_string();
        let columns = table.columns.clone();
        // Refuse building a collated index on a version-skewed table (slice 2d, collation.md §12,
        // XX002): the new B-tree would be pinned inconsistently with the file's other structures.
        self.ensure_collations_writable(&columns)?;
        // Resolve the access method (spec/design/gin.md §3): the default / `btree` is the ordered
        // B-tree, `gin` a GIN inverted index; an unknown method is 42704. Resolved here (not in the
        // parser) so the error is the resolve-time undefined_object, after the table-exists check
        // and before the column checks.
        let kind = match ci.using.as_deref().map(str::to_string).as_deref() {
            None | Some("btree") => IndexKind::Btree,
            Some("gin") => IndexKind::Gin,
            Some("gist") => IndexKind::Gist,
            Some(other) => {
                return Err(EngineError::new(
                    SqlState::UndefinedObject,
                    format!("access method does not exist: {other}"),
                ));
            }
        };
        let mut ci_keys: Vec<IndexKey> = Vec::with_capacity(ci.keys.len());
        // Host-function dependencies this index's key/predicate expressions reference
        // (spec/design/extensibility.md §8.1, step 4) — collected as each expression resolves, then
        // persisted so a reopening binary can re-check them. Empty for the common index with no host
        // function (byte-identical to v30 then).
        let mut host_deps: Vec<HostFuncDep> = Vec::new();
        let mut timezone_deps = TimeZoneDeps::default();
        for elem in &ci.keys {
            // An EXPRESSION key element (spec/design/indexes.md §1/§2): resolve it against the
            // table's columns, validate it is immutable + indexable-typed, and store its canonical
            // text (persisted, format_version 26). Expression keys are B-tree only this slice —
            // GIN/GiST take a single plain column.
            let name = match elem {
                IndexKeyElem::Expr { text, expr } => {
                    if kind != IndexKind::Btree {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            format!(
                                "an expression key on a {} index is not supported yet",
                                ci.using.as_deref().unwrap_or("")
                            ),
                        ));
                    }
                    self.admit_index_expr(table, expr, false, &mut host_deps, &mut timezone_deps)?;
                    ci_keys.push(IndexKey::Expr(IndexKeyExpr {
                        expr_text: text.clone(),
                        expr: expr.clone(),
                        unparsed: false,
                    }));
                    continue;
                }
                IndexKeyElem::Column(name) => name,
            };
            let idx = table.column_index(name).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedColumn,
                    format!("column does not exist: {name}"),
                )
            })?;
            self.check_index_column_type(kind, &columns[idx].ty)?;
            // A duplicate column in the list is ALLOWED (PostgreSQL allows it — indexes.md §1).
            ci_keys.push(IndexKey::Column(idx));
        }
        // GIN narrowings this slice (spec/design/gin.md §3): no uniqueness (undefined for an
        // inverted index) and a single column only — both deferred 0A000.
        if kind == IndexKind::Gin {
            if ci.unique {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "access method gin does not support unique indexes".to_string(),
                ));
            }
            if ci_keys.len() != 1 {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a multi-column gin index is not supported yet".to_string(),
                ));
            }
        }
        // GiST narrowings (spec/design/gist.md §1/§5/§11): no uniqueness (a bounding tree has no
        // unique key — express it as EXCLUDE (… WITH =), GX3) and a single column only (multi-column
        // GiST is GX2/GX3). File persistence (the page-5/6 R-tree + format_version 20) landed in
        // GX1b, so a file-backed GiST index is now supported; only a GiST index on a TEMP table is
        // still 0A000 (its resident R-tree would live on the temp snapshot — deferred,
        // gist.md §11), failing closed rather than silently dropping the acceleration.
        if kind == IndexKind::Gist {
            if ci.unique {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "access method gist does not support unique indexes".to_string(),
                ));
            }
            if ci_keys.len() != 1 {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a multi-column gist index is not supported yet".to_string(),
                ));
            }
            if self.is_temp_table(&ci.table) {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a gist index on a temporary table is not supported yet".to_string(),
                ));
            }
        }
        // A non-btree (GIN / GiST) index on an attached-database table is a deferred narrowing this
        // slice (attached-databases.md §8) — the attachment stores only btree PK / UNIQUE / secondary
        // indexes.
        if attach_name.is_some() && kind != IndexKind::Btree {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                format!(
                    "a {} index on an attached-database table is not supported yet",
                    ci.using.as_deref().unwrap_or("")
                ),
            ));
        }
        // The optional `WHERE predicate` making the index PARTIAL (spec/design/indexes.md §9): a
        // boolean expression over the table's own columns, validated with PG-agreeing codes. B-tree
        // only this slice (a partial GIN/GiST index is a follow-on). Validated after the key elements
        // (PG resolves the key list first) and stored as canonical text (format_version 27).
        let predicate: Option<IndexKeyExpr> = match &ci.predicate {
            None => None,
            Some(pred) => {
                if kind != IndexKind::Btree {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "a partial (WHERE) {} index is not supported yet",
                            ci.using.as_deref().unwrap_or("")
                        ),
                    ));
                }
                self.admit_index_expr(table, &pred.expr, true, &mut host_deps, &mut timezone_deps)?;
                Some(IndexKeyExpr {
                    expr_text: pred.text.clone(),
                    expr: pred.expr.clone(),
                    unparsed: false,
                })
            }
        };
        sort_host_deps(&mut host_deps);
        // `relation_taken` checks the namespace of the target scope: an attachment's OWN snapshot for an
        // attached table (each attached database is an independent namespace, §3), else the temp-aware
        // implicit namespace.
        let relation_taken = |n: &str| -> bool {
            if let Some(name) = &attach_name {
                let as_snap = self
                    .attach_read_snap(name)
                    .expect("attachment resolved above");
                as_snap.table(n).is_some()
                    || as_snap.find_index(n).is_some()
                    || as_snap.view(n).is_some()
            } else {
                self.relation_exists(n)
            }
        };
        let name = match &ci.name {
            Some(n) => {
                check_reserved_name("index", n)?;
                if relation_taken(n) {
                    // IF NOT EXISTS turns the collision into a no-op success, decided here — after
                    // the whole definition validated, before the build scan (PostgreSQL's
                    // index_create order; indexes.md §1). The jed_ reservation above still applies.
                    if ci.if_not_exists {
                        return Ok(Outcome::Statement {
                            cost: 0,
                            rows_affected: None,
                        });
                    }
                    return Err(EngineError::new(
                        SqlState::DuplicateTable,
                        format!("relation already exists: {n}"),
                    ));
                }
                n.clone()
            }
            None => {
                // PG's ChooseIndexName / ChooseIndexColumnNames (probed): exact table + one
                // name part per key element (list order; a repeated part becomes `part1`, …) +
                // "idx", truncated to fit 63 bytes, then the smallest free suffix. A column key's
                // part is the column name; a bare-function-call expression's is the function name
                // (`lower(email)` → `lower`); any other expression's is the literal `expr`
                // (indexes.md §2, grammar.md §3.1).
                let parts = index_column_names(ci.keys.iter().map(index_name_part).collect());
                let addition = name_addition(&parts);
                choose_object_name(&table_key, Some(&addition), "idx", |n| relation_taken(n))
            }
        };

        timezone_deps.validate()?;
        let def = IndexDef {
            name,
            keys: ci_keys,
            unique: ci.unique,
            kind,
            predicate,
            host_deps,
            timezone_deps,
        };
        // The build scan (cost.md §3): page_read per table-tree node + storage_row_read per
        // row. The touched set is the columns the key elements read — an index column for a
        // column key, or every column an expression key references (which may be variable-width,
        // so a spilled value adds its `value_decompress` slabs — indexes.md §5). An empty table
        // charges 0. Entries are computed here against the pre-index store; the writes below are
        // unmetered. An expression key evaluating with an error aborts the build (nothing is
        // registered — indexes.md §4), preserving all-or-nothing.
        let mut meter = self.session.new_meter();
        let entries = self.build_index_entries(ci.db.as_deref(), table, &def, &mut meter)?;
        meter.guard()?;

        let name_key = def.name.to_string();
        let ps = self.page_size;
        // Register the index catalog entry + its (empty) store in the snapshot that owns the table
        // (the resolution walk — temp-tables.md §2/§8): a session-local temp table's index lives in the
        // session temp snapshot, so the index makes ZERO file writes (the dirty bit lets the commit skip
        // the main image). The entry writes below then route through `index_store_mut`, which finds the
        // new store in that same temp snapshot (`has_index_store`) and flags the matching dirty bit.
        if let Some(name) = &attach_name {
            // The attachment's index catalog entry + (empty) store live in its working snapshot,
            // published into `Roots::attached` at commit (attached-databases.md §5/§6).
            // `attach_write_snap` clones the attachment's committed root (which carries its
            // `store_paging`) on first write and marks it dirty. Build it at the attachment's own page
            // size (§2), which may differ from main's.
            let aps = self.attach_page_size(name);
            self.attach_write_snap(name).put_index(&table_key, def, aps);
        } else if self.is_temp_table(&ci.table) {
            self.temp_working_mut().put_index(&table_key, def, ps);
        } else {
            self.working_mut().put_index(&table_key, def, ps);
        }
        let istore = self.index_store_mut_scoped(ci.db.as_deref(), &name_key);
        for ek in entries {
            if !istore.insert(ek, Vec::new())? {
                return Err(crate::error::EngineError::data_corrupted(
                    "index entry keys are unique (storage-key suffix)",
                ));
            }
        }
        Ok(Outcome::Statement {
            cost: meter.accrued,
            rows_affected: None,
        })
    }

    /// Run a REINDEX (spec/design/indexes.md §10): rebuild each target index in place from its stored
    /// definition, as if it were dropped and created again with the same name, keys, uniqueness,
    /// method, and predicate — but in one all-or-nothing statement that keeps the index's catalog
    /// position and constraint links. Every key expression and predicate is re-admitted against the
    /// handle's current environment, so the rebuilt index records the current host-function
    /// identities and timezone data (extensibility.md §8.1, index-dependencies.md). Every new tree is
    /// built before any is installed, so a failure (23505, a resolution error, the cost ceiling)
    /// changes nothing. Cost: each index charges exactly its CREATE INDEX build scan.
    pub(crate) fn execute_reindex(&mut self, rx: Reindex) -> Result<Outcome> {
        let targets = self.reindex_targets(&rx)?;
        let mut meter = self.session.new_meter();
        let mut rebuilt: Vec<(String, String, IndexDef, Vec<Vec<u8>>)> = Vec::new();
        for (scope, table_key, names) in &targets {
            if names.is_empty() {
                continue;
            }
            self.check_attachment_writable(Some(scope))?;
            let table = self
                .snap_for_scope(scope)
                .and_then(|s| s.table(table_key))
                .expect("a REINDEX target table resolved above");
            self.ensure_collations_writable(&table.columns)?;
            for name in names {
                let old = table
                    .indexes
                    .iter()
                    .find(|i| &i.name == name)
                    .expect("a REINDEX target index belongs to its table");
                let def = self.readmit_index_def(table, old)?;
                let entries = self.build_index_entries(Some(scope), table, &def, &mut meter)?;
                rebuilt.push((scope.clone(), table_key.clone(), def, entries));
            }
        }
        meter.guard()?;
        for (scope, table_key, def, entries) in rebuilt {
            let name_key = def.name.to_string();
            let ps = match scope.as_str() {
                "main" | "temp" => self.page_size,
                other => self.attach_page_size(other),
            };
            let snap = match scope.as_str() {
                "temp" => self.temp_working_mut(),
                "main" => self.working_mut(),
                other => {
                    let other = other.to_string();
                    self.attach_write_snap(&other)
                }
            };
            snap.remove_index(&table_key, &name_key);
            snap.put_index(&table_key, def, ps);
            let istore = self.index_store_mut_scoped(Some(&scope), &name_key);
            for ek in entries {
                if !istore.insert(ek, Vec::new())? {
                    return Err(crate::error::EngineError::data_corrupted(
                        "index entry keys are unique (storage-key suffix)",
                    ));
                }
            }
        }
        Ok(Outcome::Statement {
            cost: meter.accrued,
            rows_affected: None,
        })
    }

    /// Re-admit a stored index definition for REINDEX (spec/design/indexes.md §10): an unparsed key
    /// or predicate is XX003 (unparsable-definitions.md §3.4); every expression is resolved again and
    /// its host-function and timezone dependencies re-collected from the current environment exactly
    /// as CREATE INDEX collects them (a missing function 42883, a no-longer-admissible one 42P17, an
    /// unloaded named zone 22023). Column keys, uniqueness, method, and predicate text carry over.
    fn readmit_index_def(&self, table: &Table, old: &IndexDef) -> Result<IndexDef> {
        let unparsed = old
            .keys
            .iter()
            .any(|k| matches!(k, IndexKey::Expr(e) if e.unparsed))
            || old.predicate.as_ref().is_some_and(|p| p.unparsed);
        if unparsed {
            return Err(crate::catalog::unparsable_index_error(
                &table.name,
                &old.name,
            ));
        }
        let mut host_deps: Vec<HostFuncDep> = Vec::new();
        let mut timezone_deps = TimeZoneDeps::default();
        for k in &old.keys {
            if let IndexKey::Expr(e) = k {
                self.admit_index_expr(table, &e.expr, false, &mut host_deps, &mut timezone_deps)?;
            }
        }
        if let Some(pred) = &old.predicate {
            self.admit_index_expr(table, &pred.expr, true, &mut host_deps, &mut timezone_deps)?;
        }
        sort_host_deps(&mut host_deps);
        timezone_deps.validate()?;
        let mut def = old.clone();
        def.host_deps = host_deps;
        def.timezone_deps = timezone_deps;
        Ok(def)
    }

    /// Resolve a REINDEX's targets (spec/design/indexes.md §10) as `(scope, table key, index names)`
    /// triples, each scope explicit (`main` / `temp` / an attachment) and each table's indexes in
    /// catalog (name byte) order. INDEX names one index (42P01 when no relation has the name, 42809
    /// when it is not an index); TABLE names one table (42809 for a catalog relation, an index, a view,
    /// or a sequence; 42P01 when missing); DATABASE takes every table of the named database in name
    /// byte order — `main` (the default) followed by the session's temporary tables, or one attachment;
    /// SYSTEM has no system catalogs to rebuild. Any other database name is 0A000, as PostgreSQL.
    fn reindex_targets(&self, rx: &Reindex) -> Result<Vec<(String, String, Vec<String>)>> {
        let index_names =
            |t: &Table| -> Vec<String> { t.indexes.iter().map(|i| i.name.to_string()).collect() };
        match rx.kind {
            ReindexKind::Index | ReindexKind::Table => {
                let name = rx.name.as_deref().expect("INDEX/TABLE carry a name");
                if rx.kind == ReindexKind::Table {
                    check_catalog_rel_write(name)?;
                }
                let scope = match rx.db.as_deref() {
                    Some(q) => {
                        let q = q.to_string();
                        if q != "main" && q != "temp" && self.attach_read_snap(&q).is_none() {
                            return Err(EngineError::new(
                                SqlState::UndefinedTable,
                                format!("database \"{q}\" is not attached"),
                            ));
                        }
                        q
                    }
                    // The resolution walk: a name in the session's temporary domain (a table, an
                    // index, or an owned sequence — preclude-overlaps keeps it in one scope), else main.
                    None if self.is_temp_table(name)
                        || self.is_temp_index(name)
                        || self.is_temp_sequence(name) =>
                    {
                        "temp".to_string()
                    }
                    None => "main".to_string(),
                };
                let snap = self.snap_for_scope(&scope).expect("scope validated above");
                let shown = match &rx.db {
                    Some(q) => format!("{q}.{name}"),
                    None => name.to_string(),
                };
                let found = if rx.kind == ReindexKind::Index {
                    snap.find_index(name)
                        .map(|(tk, _)| (tk.to_string(), vec![name.to_string()]))
                } else {
                    snap.table(name)
                        .map(|t| (t.name.to_string(), index_names(t)))
                };
                if let Some((table_key, names)) = found {
                    return Ok(vec![(scope, table_key, names)]);
                }
                let other_kind = snap.table(name).is_some()
                    || snap.find_index(name).is_some()
                    || snap.view(name).is_some()
                    || snap.sequence(name).is_some()
                    || (rx.kind == ReindexKind::Index && is_catalog_rel_name(name));
                if other_kind {
                    let what = if rx.kind == ReindexKind::Index {
                        "an index"
                    } else {
                        "a table or materialized view"
                    };
                    return Err(EngineError::new(
                        SqlState::WrongObjectType,
                        format!("\"{shown}\" is not {what}"),
                    ));
                }
                Err(EngineError::new(
                    SqlState::UndefinedTable,
                    format!("relation \"{shown}\" does not exist"),
                ))
            }
            ReindexKind::Database | ReindexKind::System => {
                if rx.kind == ReindexKind::System && rx.concurrently {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "cannot reindex system catalogs concurrently",
                    ));
                }
                let scope = match rx.name.as_deref() {
                    None => "main".to_string(),
                    Some(n) => {
                        let n = n.to_string();
                        if n != "main" && (n == "temp" || self.attach_read_snap(&n).is_none()) {
                            return Err(EngineError::new(
                                SqlState::FeatureNotSupported,
                                "can only reindex the currently open database",
                            ));
                        }
                        n
                    }
                };
                if rx.kind == ReindexKind::System {
                    return Ok(Vec::new());
                }
                let mut scopes = vec![scope.clone()];
                if scope == "main" {
                    scopes.push("temp".to_string());
                }
                let mut out = Vec::new();
                for sc in scopes {
                    let snap = self.snap_for_scope(&sc).expect("scope validated above");
                    for t in snap.tables_sorted() {
                        out.push((sc.clone(), t.name.to_string(), index_names(t)));
                    }
                }
                Ok(out)
            }
        }
    }

    /// Resolve one index key expression (`predicate` false) or partial-index predicate (`predicate`
    /// true) against `table` and admit it for a persisted index (spec/design/indexes.md §1/§2/§9):
    /// a subquery is 0A000 and a bind parameter 42P02 (structural pre-walk); an aggregate 42803 /
    /// window 42P20 / non-boolean predicate 42804 fall out of the resolver; a non-immutable operation
    /// is 42P17; a key's result type must be key-encodable (0A000). The host-function and timezone
    /// dependencies it reaches accumulate into `host_deps` / `tz` (extensibility.md §8.1,
    /// index-dependencies.md). Shared by CREATE INDEX and REINDEX (§10), which re-admits every stored
    /// expression against the current environment.
    fn admit_index_expr(
        &self,
        table: &Table,
        expr: &Expr,
        predicate: bool,
        host_deps: &mut Vec<HostFuncDep>,
        tz: &mut TimeZoneDeps,
    ) -> Result<()> {
        // A subquery is not a deterministic function of the row — 0A000 (the resolver admits an
        // uncorrelated one, so it is rejected here, before resolution).
        reject_index_predicate_structure(expr)?;
        let scope = Scope::index(self, table);
        let mut pt = ParamTypes::for_index();
        let node = if predicate {
            let node = resolve_boolean_filter(&scope, expr, &mut pt)?;
            if pt.nonimmutable {
                return Err(EngineError::new(
                    SqlState::InvalidObjectDefinition,
                    "functions in index predicate must be marked IMMUTABLE".to_string(),
                ));
            }
            node
        } else {
            let (node, rtype) = resolve(&scope, expr, None, &mut AggCtx::Forbidden, &mut pt)?;
            // Dependencies accumulate at each resolved operation, through every child.
            if pt.nonimmutable {
                return Err(EngineError::new(
                    SqlState::InvalidObjectDefinition,
                    "functions in index expression must be marked IMMUTABLE".to_string(),
                ));
            }
            // The result type must be key-encodable (a composite result is 0A000).
            if resolved_to_key_type(&rtype).is_none() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "an index on an expression of this result type is not supported yet"
                        .to_string(),
                ));
            }
            node
        };
        // Collect + admit any host functions the expression calls (§8.1, step 4): each must be
        // IMMUTABLE + carry a component identity, else 42P17 (this also closes the latent leak where
        // a volatile host function passed the syntactic immutability walk).
        collect_index_host_deps(&node, self.session.env.functions(), host_deps)?;
        tz.merge(&pt.timezone_deps);
        Ok(())
    }

    /// Compute a new index's entry keys from `table`'s current rows (the CREATE INDEX build, shared
    /// with REINDEX — spec/design/indexes.md §2/§10), sorted by entry key. The build scan is metered
    /// on `meter` (cost.md §3): page_read per table-tree node + storage_row_read per row, over the
    /// touched set of columns the keys and predicate read (a spilled value adds its
    /// `value_decompress` slabs). A UNIQUE definition verifies the rows (23505, indexes.md §8). An
    /// expression key evaluating with an error aborts the build, so nothing is registered.
    pub(crate) fn build_index_entries(
        &self,
        scope: Option<&str>,
        table: &Table,
        def: &IndexDef,
        meter: &mut Meter,
    ) -> Result<Vec<Vec<u8>>> {
        let columns = table.columns.clone();
        let colls = self.column_collations(&columns);
        let mut mask = vec![false; columns.len()];
        for k in &def.keys {
            match k {
                IndexKey::Column(c) => mask[*c] = true,
                IndexKey::Expr(e) => {
                    for c in check_referenced_columns(&e.expr, &columns) {
                        mask[c] = true;
                    }
                }
            }
        }
        // A partial index's predicate is evaluated per row during the build (indexes.md §9), so the
        // columns it references join the touched set — the scan reads (and, if spilled, decompresses)
        // them, keeping the build cost deterministic and cross-core identical.
        if let Some(pred) = &def.predicate {
            for c in check_referenced_columns(&pred.expr, &columns) {
                mask[c] = true;
            }
        }
        // Resolve the index once (column ordinals + resolved expression keys); the eval env for any
        // expression key (a fresh statement rng — index expressions are immutable, so it is never
        // read). Built before the `&mut self` writes below, so the `&self` borrow is released first.
        let rindex = self.resolve_index(table, def)?;
        let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let mut entries: Vec<Vec<u8>> = Vec::new();
        // A UNIQUE build verifies the existing rows before the index is registered
        // (indexes.md §8): two rows sharing a fully-non-NULL key tuple — i.e. an exempt-free
        // prefix — trap 23505 and create nothing. Unmetered validation (cost.md §3).
        let mut seen_prefixes: HashSet<Vec<u8>> = HashSet::new();
        {
            let env = EvalEnv {
                exec: self,
                params: &[],
                outer: &[],
                rng: &rng,
                ctes: CteCtx::empty(),
                host_batch: None,
            };
            let store = self.store_scoped(scope, &table.name);
            // Resolved column types (composite index key vehicle, §2.15).
            let col_types: Vec<ColType> = store.col_types().to_vec();
            let (table_entries, nodes, slabs) = store.scan_with_units(&mask)?;
            meter.charge(COSTS.page_read * nodes as i64 + COSTS.value_decompress * slabs as i64);
            entries.reserve(table_entries.len());
            for (key, mut row) in table_entries {
                meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
                meter.charge(COSTS.storage_row_read);
                // Resolve a faulted row's touched columns before encoding (an expression key may
                // read a spilled value; the evaluator's `Unfetched` backstop also handles it).
                store.resolve_inline_columns(&mut row)?;
                if def.unique
                    && let Some(prefix) = index_prefix_key(&col_types, &colls, &rindex, &row, &env)?
                    && !seen_prefixes.insert(prefix)
                {
                    return Err(
                        EngineError::unique_violation(&table.name, &def.name).with_detail(
                            self.index_detail(
                                &table.name,
                                &columns,
                                &def,
                                &rindex,
                                &row,
                                &env,
                                DUPLICATED,
                            ),
                        ),
                    );
                }
                entries.extend(index_entry_keys(
                    &columns, &col_types, &colls, &rindex, &key, &row, &env,
                )?);
            }
        }
        meter.guard()?;
        // Sorted by entry key (indexes.md §1): every insert is then a right-edge append, so the built
        // tree packs ~full instead of splintering under the storage-key order the scan produced
        // (random in entry-key space). Part of the byte contract — the sort fixes the built tree's
        // shape across cores.
        entries.sort_unstable();
        Ok(entries)
    }

    /// Run a DROP INDEX (spec/design/indexes.md §2): a table's name is 42809, a missing one
    /// 42704. A pure catalog edit — zero cost, like DROP TABLE. The index is resolved along the
    /// resolution walk (session-local → persistent — temp-tables.md §8) and removed from the snapshot
    /// that owns it, so dropping a temp table's index makes zero file writes.
    pub(crate) fn execute_drop_index(&mut self, di: DropIndex) -> Result<Outcome> {
        // `table` covers both scopes, so DROP INDEX naming a table is 42809 regardless of kind.
        if self.table(&di.name).is_some() || self.is_view(&di.name) {
            return Err(EngineError::new(
                SqlState::WrongObjectType,
                format!("{} is not an index", di.name),
            ));
        }
        let name_key = di.name.to_string();
        // An index that backs an EXCLUDE constraint cannot be dropped directly — the constraint owns
        // it (the UNIQUE-backing precedent; jed has no ALTER TABLE … DROP CONSTRAINT yet, so the
        // index lives until DROP TABLE). 2BP01, matching PG's "cannot drop index … because
        // constraint … requires it" (spec/design/gist.md §7).
        if let Some(table_key) = self.find_index(&di.name).map(|(tk, _)| tk.to_string()) {
            if let Some(t) = self.table(&table_key) {
                if t.exclusions.iter().any(|e| e.index == di.name) {
                    return Err(EngineError::new(
                        SqlState::DependentObjectsStillExist,
                        format!(
                            "cannot drop index {} because constraint {} on table {} requires it",
                            di.name, di.name, t.name
                        ),
                    )
                    .hinted(&error_hint::drop_owner_instead(&di.name, &t.name)));
                }
            }
        }
        if self.is_temp_index(&di.name) {
            let table_key = self
                .temp_read_snap()
                .find_index(&di.name)
                .unwrap()
                .0
                .to_string();
            self.temp_working_mut().remove_index(&table_key, &name_key);
        } else if let Some((table_key, _)) = self.find_index(&di.name) {
            let table_key = table_key.to_string();
            self.working_mut().remove_index(&table_key, &name_key);
        } else {
            return Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("index does not exist: {}", di.name),
            ));
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a CREATE TYPE (spec/design/composite.md): reject a duplicate type name
    /// (42710), resolve each field's type (a built-in scalar, or a *previously-defined* composite
    /// — 42704 if unknown; no self- or forward-reference), reject a duplicate field name (42701),
    /// then register the composite type in the catalog. Named composites only.
    pub(crate) fn execute_create_type(&mut self, ct: CreateType) -> Result<Outcome> {
        check_reserved_name("type", &ct.name)?;
        if self.read_snap().composite_type(&ct.name).is_some() {
            return Err(EngineError::new(
                SqlState::DuplicateObject,
                format!("type {} already exists", ct.name),
            ));
        }
        let mut fields: Vec<CompositeField> = Vec::with_capacity(ct.fields.len());
        for f in &ct.fields {
            if fields.iter().any(|g| g.name == f.name) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("attribute {} specified more than once", f.name),
                ));
            }
            let mut ftemporal: Option<TemporalTypmod> = None;
            let (fty, fdecimal, fvarchar): (Type, Option<DecimalTypmod>, Option<u32>) =
                if let Some(base) = f.type_name.strip_suffix("[]") {
                    // An array-typed field (spec/design/array.md §12 — the mirror of an
                    // array-of-composite element). The element is a scalar or a *previously-defined*
                    // composite (`element_type_code` 14 + name on disk); a nested-array element and
                    // an array typmod (`numeric(p,s)[]`) stay deferred (0A000), exactly as for an
                    // array column.
                    if f.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "a type modifier on an array type is not supported yet".to_string(),
                        ));
                    }
                    let elem = if let Some(s) = ScalarType::from_name(base) {
                        Type::Scalar(s)
                    } else if let Some(ctype) = self.read_snap().composite_type(base) {
                        Type::Composite(crate::types::CompositeRef {
                            name: ctype.name.clone(),
                        })
                    } else {
                        return Err(EngineError::new(
                            SqlState::UndefinedObject,
                            format!("type does not exist: {base}"),
                        ));
                    };
                    (Type::Array(Box::new(elem)), None, None)
                } else if ScalarType::from_name(&f.type_name).is_some() {
                    let (s, tm) = resolve_type_and_typmod(&f.type_name, &f.type_mod)?;
                    ftemporal = tm.temporal;
                    (Type::Scalar(s), tm.decimal, tm.varchar_len)
                } else if let Some(rdesc) = crate::range::range_by_name(&f.type_name) {
                    // A range-typed field (spec/design/composite.md §3, format_version 41): the
                    // element is carried inline like a range column's, and a range takes no typmod.
                    if f.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "a type modifier on a range type is not supported".to_string(),
                        ));
                    }
                    let elem = crate::range::element_scalar(rdesc);
                    (Type::Range(Box::new(Type::Scalar(elem))), None, None)
                } else if self.read_snap().composite_type(&f.type_name).is_some() {
                    if f.type_mod.is_some() {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            format!(
                                "a type modifier is not supported for composite type {}",
                                f.type_name
                            ),
                        ));
                    }
                    (
                        Type::Composite(crate::types::CompositeRef {
                            name: f.type_name.clone(),
                        }),
                        None,
                        None,
                    )
                } else {
                    return Err(EngineError::new(
                        SqlState::UndefinedObject,
                        format!("type does not exist: {}", f.type_name),
                    ));
                };
            fields.push(CompositeField {
                name: f.name.clone(),
                ty: fty,
                decimal: fdecimal,
                varchar_len: fvarchar,
                temporal: ftemporal,
                not_null: f.not_null,
            });
        }
        // Bound composite-type nesting depth (CLAUDE.md §13; cost.md §7b). A chain of CREATE TYPEs
        // each nesting the previous (`a`, `b AS (x a)`, …) builds unbounded depth across many cheap
        // statements — invisible to the per-statement input-size cap and the parser nesting counter —
        // and every derived recursive walk (codec, comparator, record_out/in, resolve_col_type)
        // recurses to this depth. Reject at the producer so no over-deep type enters the catalog and
        // every downstream walk stays stack-safe. Fields reference only existing types (each already
        // ≤ MAX_COMPOSITE_DEPTH), so this depth computation's recursion is itself bounded.
        let mut cache: HashMap<String, usize> = HashMap::new();
        let mut max_field = 0;
        for f in &fields {
            max_field = max_field.max(self.read_snap().composite_type_depth(&f.ty, &mut cache));
        }
        let depth = 1 + max_field;
        if depth > MAX_COMPOSITE_DEPTH {
            return Err(EngineError::new(
                SqlState::StatementTooComplex,
                format!(
                    "composite type {} nesting depth {depth} exceeds the maximum of {MAX_COMPOSITE_DEPTH}",
                    ct.name
                ),
            ));
        }
        self.working_mut().put_type(CompositeType {
            name: ct.name.clone(),
            fields,
        });
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a DROP TYPE (spec/design/composite.md §7). RESTRICT (the only behavior
    /// this slice): a missing type is 42704 unless `IF EXISTS`; if any table column or composite
    /// field still references the type, 2BP01; otherwise remove it from the catalog.
    pub(crate) fn execute_drop_type(&mut self, dt: DropType) -> Result<Outcome> {
        if self.read_snap().composite_type(&dt.name).is_none() {
            if dt.if_exists {
                return Ok(Outcome::Statement {
                    cost: 0,
                    rows_affected: None,
                });
            }
            return Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("type does not exist: {}", dt.name),
            ));
        }
        if let Some(dep) = self.composite_dependent_any(&dt.name) {
            return Err(EngineError::new(
                SqlState::DependentObjectsStillExist,
                format!(
                    "cannot drop type {} because other objects depend on it: {}",
                    dt.name, dep
                ),
            ));
        }
        let key = dt.name.to_string();
        self.working_mut().remove_type(&key);
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a CREATE SEQUENCE (spec/design/sequences.md). Resolve the option overrides
    /// against the INCREMENT sign's type defaults, validate the set (22023), reject a relation-
    /// namespace collision (42P07 unless `IF NOT EXISTS`), and register the sequence.
    pub(crate) fn execute_create_sequence(&mut self, cs: CreateSequence) -> Result<Outcome> {
        // The reservation is not a collision, so IF NOT EXISTS does not suppress it
        // (spec/design/introspection.md §4).
        check_reserved_name("sequence", &cs.name)?;
        if self.relation_exists(&cs.name) {
            if cs.if_not_exists {
                return Ok(Outcome::Statement {
                    cost: 0,
                    rows_affected: None,
                });
            }
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation already exists: {}", cs.name),
            ));
        }
        if cs.options.sequence_name.is_some() {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "invalid sequence option SEQUENCE NAME".to_string(),
            ));
        }
        let def = build_sequence_def(&cs.name, &cs.options, None)?;
        self.working_mut().put_sequence(def);
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run a DROP SEQUENCE (spec/design/sequences.md §1). RESTRICT-only: a missing
    /// sequence is 42P01 unless `IF EXISTS`. No dependency tracking this slice (a plain `DEFAULT
    /// nextval('s')` creates none — PG). Multiple names are dropped left to right.
    pub(crate) fn execute_drop_sequence(&mut self, ds: DropSequence) -> Result<Outcome> {
        for name in &ds.names {
            // Missing → 42P01 (unless IF EXISTS). An OWNED (serial) sequence has a dependent — its
            // column's default — so RESTRICT (the only mode this slice; CASCADE 0A000) is 2BP01
            // (spec/design/sequences.md §12). Clone the owner ref out so the snapshot borrow ends
            // before the working-snapshot mutation.
            let owner = match self.sequence(name) {
                None => {
                    if ds.if_exists {
                        continue;
                    }
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("sequence does not exist: {name}"),
                    ));
                }
                Some(s) => s
                    .owned_by
                    .as_ref()
                    .map(|o| (s.name.clone(), o.table.clone(), o.column)),
            };
            if let Some((seq_name, owner_table, owner_col)) = owner {
                // The owning table is always present (its own DROP TABLE would auto-drop this
                // sequence first), so the column name for the detail resolves. The scope-aware
                // `table` funnel finds an owned TEMP sequence's temp owner (temp-tables.md §8).
                let (col_name, table_name) = self
                    .table(&owner_table)
                    .map(|t| {
                        (
                            t.columns
                                .get(owner_col as usize)
                                .map_or_else(String::new, |c| c.name.clone()),
                            t.name.clone(),
                        )
                    })
                    .unwrap_or_else(|| (String::new(), owner_table.clone()));
                return Err(EngineError::new(
                    SqlState::DependentObjectsStillExist,
                    format!(
                        "cannot drop sequence {seq_name} because other objects depend on it: default value for column {col_name} of table {table_name} depends on sequence {seq_name}"
                    ),
                ));
            }
            // Not owned: remove from whichever scope owns it (a temp sequence is always owned, so this
            // routed path is reached only for a plain persistent sequence — temp-tables.md §8).
            self.remove_sequence_routed(name);
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Analyze and run an `ALTER SEQUENCE [IF EXISTS] s <action>` (spec/design/sequences.md §4/§15).
    /// A missing sequence is 42P01 unless `IF EXISTS` (then a no-op). The option form re-edits the
    /// definition (PG `init_params`, `isInit = false` — only written options change, the counter is
    /// preserved unless `RESTART`); `RENAME TO` moves the catalog key. Touches no session state
    /// (`currval`/`lastval` unchanged). A catalog write (the write path, transactional, §5).
    pub(crate) fn execute_alter_sequence(&mut self, als: AlterSequence) -> Result<Outcome> {
        let existing = match self.sequence(&als.name) {
            Some(d) => d.clone(),
            None => {
                if als.if_exists {
                    return Ok(Outcome::Statement {
                        cost: 0,
                        rows_affected: None,
                    });
                }
                return Err(EngineError::new(
                    SqlState::UndefinedTable,
                    format!("relation does not exist: {}", als.name),
                ));
            }
        };
        match als.action {
            AlterSeqAction::SetOptions { options, restart } => {
                // `AS type` on ALTER is 0A000 — the value type is not persisted (sequences.md §14.4),
                // so the original type for re-deriving a default bound is gone.
                if options.data_type.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "ALTER SEQUENCE ... AS type is not supported".to_string(),
                    ));
                }
                if options.sequence_name.is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        "invalid sequence option SEQUENCE NAME".to_string(),
                    ));
                }
                let new_def = apply_seq_alter(&existing, &options, restart, SeqDataType::BigInt)?;
                self.put_sequence_routed(new_def);
            }
            AlterSeqAction::Rename(new_name) => {
                self.alter_sequence_rename(&existing, &new_name)?;
            }
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// `ALTER SEQUENCE s RENAME TO s2` (spec/design/sequences.md §15.3): a collision with any
    /// relation — including `s` itself — is 42P07; otherwise move the entry to the new key. For an
    /// **owned** sequence, the owning column's `DEFAULT nextval('s')` text is rewritten to
    /// `nextval('s2')` so a later INSERT still advances the renamed sequence (jed resolves the
    /// sequence by name, unlike PG's OID reference).
    pub(crate) fn alter_sequence_rename(
        &mut self,
        existing: &SequenceDef,
        new_name: &str,
    ) -> Result<()> {
        check_reserved_name("sequence", new_name)?;
        if self.relation_exists(new_name) {
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation already exists: {new_name}"),
            ));
        }
        // Rewrite the owning column's nextval default in place (an owned sequence only) — the rows
        // and store must survive, so this mutates the catalog column, not via `put_table`. The owner
        // table is always present (its DROP TABLE would have auto-dropped this sequence first).
        if let Some(owner) = &existing.owned_by {
            let expr_text = nextval_default_text(new_name);
            let expr = crate::parser::parse_expression(&expr_text)?;
            // Route to the owner's scope so a renamed owned TEMP sequence rewrites its column default
            // in the temp snapshot (temp-tables.md §8).
            self.set_column_default_expr_routed(
                &owner.table.to_string(),
                owner.column as usize,
                DefaultExpr {
                    expr_text,
                    expr,
                    unparsed: false,
                },
            );
        }
        // Capture the owning scope BEFORE the remove: after dropping the old key the new name is in no
        // scope, so a post-remove route would wrongly default to the main image (temp-tables.md §8).
        let is_temp = self.is_temp_sequence(&existing.name);
        let old_key = existing.name.to_string();
        let mut def = existing.clone();
        def.name = new_name.to_string();
        let w = if is_temp {
            self.temp_working_mut()
        } else {
            self.working_mut()
        };
        w.remove_sequence(&old_key);
        w.put_sequence(def);
        Ok(())
    }
}

#[cfg(test)]
mod expr_index_tests {
    //! Expression-index behaviors the shared corpus cannot express (a PG divergence — jed's
    //! text-key collation is C, not the oracle's; on-disk byte round-trip; catalog introspection).
    //! The PG-agreeing behavior (23505, error codes, planner rows) lives in the corpus.
    use crate::{Engine, Outcome, execute};

    fn rows(db: &mut Engine, sql: &str) -> Vec<Vec<crate::Value>> {
        match execute(db, sql).expect(sql) {
            Outcome::Query { rows, .. } => rows,
            other => panic!("expected a query, got {other:?} for {sql}"),
        }
    }

    // A UNIQUE expression index enforces `lower(email)` uniqueness across INSERTs, and survives a
    // serialize→load round trip (format_version 26): the reloaded index still enforces + accelerates.
    #[test]
    fn unique_lower_email_enforced_and_persisted() {
        let mut db = Engine::new();
        execute(&mut db, "CREATE TABLE u (id i32 PRIMARY KEY, email text)").unwrap();
        execute(&mut db, "CREATE UNIQUE INDEX ON u (lower(email))").unwrap();
        execute(&mut db, "INSERT INTO u VALUES (1, 'Alice@X')").unwrap();
        // A case-different duplicate collides on lower(email) (23505).
        let e = execute(&mut db, "INSERT INTO u VALUES (2, 'ALICE@x')").unwrap_err();
        assert_eq!(e.code(), "23505", "case-insensitive uniqueness");
        // A distinct value inserts fine.
        execute(&mut db, "INSERT INTO u VALUES (2, 'bob@x')").unwrap();

        // Round-trip: the v26 catalog re-parses the index expression, and it still enforces.
        let image = db.to_image(256, 1).unwrap();
        let mut re = Engine::from_image(&image).unwrap();
        let dup = execute(&mut re, "INSERT INTO u VALUES (3, 'aLICE@x')").unwrap_err();
        assert_eq!(dup.code(), "23505", "uniqueness survives reload");
        // And the accelerated lookup returns the row.
        let r = rows(&mut re, "SELECT id FROM u WHERE lower(email) = 'alice@x'");
        assert_eq!(r.len(), 1, "one row matches lower(email)='alice@x'");
    }

    // A plain expression index is used by the planner (EXPLAIN names it) and updated across
    // INSERT/UPDATE/DELETE so the accelerated query stays correct.
    #[test]
    fn plain_expr_index_planner_and_maintenance() {
        let mut db = Engine::new();
        execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)").unwrap();
        execute(&mut db, "CREATE INDEX ON t ((a + b))").unwrap();
        // ids 2 and 3 both have a+b = 10 (4+6, 5+5); id 1 has a+b = 5.
        execute(
            &mut db,
            "INSERT INTO t VALUES (1, 2, 3), (2, 4, 6), (3, 5, 5)",
        )
        .unwrap();
        // The access-predicate bound names the auto-named expression index (a+b → `t_expr_idx`).
        // The EXPLAIN plan's `detail` column (r[2]) carries "Index bound: using <name>".
        let plan = match execute(&mut db, "EXPLAIN SELECT id FROM t WHERE a + b = 10").unwrap() {
            Outcome::Query { rows, .. } => rows
                .iter()
                .map(|r| format!("{:?}", r[2]))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => unreachable!(),
        };
        assert!(
            plan.contains("t_expr_idx"),
            "plan should name the expr index:\n{plan}"
        );
        // The query returns the two rows whose a+b = 10 (ids 2 and 3), regardless of pushdown.
        let mut ids: Vec<i64> = rows(&mut db, "SELECT id FROM t WHERE a + b = 10")
            .iter()
            .map(|r| match r[0] {
                crate::Value::Int(n) => n,
                _ => panic!(),
            })
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![2, 3]);
        // UPDATE moves an entry; DELETE removes one — the index stays consistent.
        execute(&mut db, "UPDATE t SET a = 100 WHERE id = 2").unwrap(); // a+b now 110
        execute(&mut db, "DELETE FROM t WHERE id = 3").unwrap();
        let after = rows(&mut db, "SELECT id FROM t WHERE a + b = 10");
        assert!(after.is_empty(), "no rows with a+b=10 after update/delete");
    }

    // Non-immutable / non-indexable expression keys are rejected at CREATE INDEX (the exact codes
    // are corpus-checked against PG; here we pin the jed-specific 42P17 immutability rule).
    #[test]
    fn nonimmutable_expression_rejected() {
        let mut db = Engine::new();
        execute(
            &mut db,
            "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz, a i32)",
        )
        .unwrap();
        let e = execute(&mut db, "CREATE INDEX ON t ((uuidv4()))").unwrap_err();
        assert_eq!(e.code(), "42P17", "seam function rejected");
        // A cast that consults the session timezone is nonimmutable (index-dependencies.md).
        let e2 = execute(&mut db, "CREATE INDEX ON t ((ts::date))").unwrap_err();
        assert_eq!(e2.code(), "42P17", "timestamptz expression rejected");
        // A bare `(ts)` normalizes to a plain column key — a timestamptz COLUMN is indexable.
        execute(&mut db, "CREATE INDEX ON t ((ts))").expect("bare (ts) is a column key");
    }

    // jed_indexes shows an expression key as its canonical text in the `columns` array.
    #[test]
    fn introspection_shows_expression_text() {
        let mut db = Engine::new();
        execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, email text)").unwrap();
        execute(&mut db, "CREATE INDEX ix ON t (lower(email))").unwrap();
        let r = rows(&mut db, "SELECT columns FROM jed_indexes WHERE name = 'ix'");
        assert_eq!(r.len(), 1);
        // columns is text[] = {'lower ( email )'} (the canonical Check-expression text form).
        let s = format!("{:?}", r[0][0]);
        assert!(
            s.contains("lower"),
            "columns should carry the expression text: {s}"
        );
    }
}

#[cfg(test)]
mod partial_index_tests {
    //! Partial-index behaviors the shared corpus cannot express (a PG divergence — jed's syntactic
    //! implication; on-disk byte round-trip; catalog introspection). The
    //! PG-agreeing behavior (23505 among qualifying rows, error codes, planner rows) lives in the
    //! corpus (spec/conformance/suites/ddl/partial_index.test).
    use crate::{Engine, Outcome, execute};

    fn rows(db: &mut Engine, sql: &str) -> Vec<Vec<crate::Value>> {
        match execute(db, sql).expect(sql) {
            Outcome::Query { rows, .. } => rows,
            other => panic!("expected a query, got {other:?} for {sql}"),
        }
    }

    // A UNIQUE partial index constrains ONLY its qualifying rows (indexes.md §9): two `active` rows
    // may not share `amt`, but an `inactive` row may duplicate an `active` one. Survives reload (v27).
    #[test]
    fn partial_unique_constrains_only_qualifying_and_persists() {
        let mut db = Engine::new();
        execute(
            &mut db,
            "CREATE TABLE pt (id i32 PRIMARY KEY, status text, amt i32)",
        )
        .unwrap();
        execute(&mut db, "INSERT INTO pt VALUES (1, 'active', 10)").unwrap();
        execute(
            &mut db,
            "CREATE UNIQUE INDEX pt_uact ON pt (amt) WHERE status = 'active'",
        )
        .unwrap();
        // An inactive row may duplicate the active amt=10 (it is not in the index).
        execute(&mut db, "INSERT INTO pt VALUES (2, 'inactive', 10)").unwrap();
        // A second active amt=10 collides (23505 names the partial index).
        let e = execute(&mut db, "INSERT INTO pt VALUES (3, 'active', 10)").unwrap_err();
        assert_eq!(e.code(), "23505", "two active rows may not share amt");
        // Round-trip: the v27 catalog re-parses the predicate, and it still enforces + exempts.
        let image = db.to_image(256, 1).unwrap();
        let mut re = Engine::from_image(&image).unwrap();
        execute(&mut re, "INSERT INTO pt VALUES (4, 'inactive', 10)")
            .expect("inactive dup still allowed after reload");
        let dup = execute(&mut re, "INSERT INTO pt VALUES (5, 'active', 10)").unwrap_err();
        assert_eq!(dup.code(), "23505", "partial uniqueness survives reload");
    }

    // The planner uses a partial index ONLY when the WHERE contains the predicate conjunct
    // (indexes.md §9) — the syntactic implication gate. EXPLAIN names it when gated, not otherwise.
    #[test]
    fn planner_gates_on_predicate_conjunct() {
        let mut db = Engine::new();
        execute(
            &mut db,
            "CREATE TABLE pt (id i32 PRIMARY KEY, status text, amt i32)",
        )
        .unwrap();
        execute(
            &mut db,
            "INSERT INTO pt VALUES (1,'active',10),(2,'inactive',10),(3,'active',30)",
        )
        .unwrap();
        execute(
            &mut db,
            "CREATE INDEX pt_amt_active ON pt (amt) WHERE status = 'active'",
        )
        .unwrap();
        let plan = |db: &mut Engine, sql: &str| match execute(db, sql).unwrap() {
            Outcome::Query { rows, .. } => rows
                .iter()
                .map(|r| format!("{:?}", r[2]))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => unreachable!(),
        };
        // Gated: the WHERE contains `status = 'active'` (the predicate), so the index is used.
        let gated = plan(
            &mut db,
            "EXPLAIN SELECT id FROM pt WHERE status = 'active' AND amt = 10",
        );
        assert!(
            gated.contains("pt_amt_active"),
            "gated plan should use the partial index:\n{gated}"
        );
        // Ungated: no predicate conjunct → full scan (the index is NOT named).
        let ungated = plan(&mut db, "EXPLAIN SELECT id FROM pt WHERE amt = 10");
        assert!(
            !ungated.contains("pt_amt_active"),
            "ungated plan must NOT use the partial index:\n{ungated}"
        );
        // Rows are correct either way (the residual filter re-applies the full WHERE).
        let ids: Vec<i64> = rows(
            &mut db,
            "SELECT id FROM pt WHERE status = 'active' AND amt = 10",
        )
        .iter()
        .map(|r| match r[0] {
            crate::Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
        assert_eq!(ids, vec![1], "only the active amt=10 row");
    }

    // A session-dependent timestamptz-to-date cast is 42P17; a non-boolean predicate is
    // 42804; a partial GIN index is 0A000.
    #[test]
    fn predicate_rejections() {
        let mut db = Engine::new();
        execute(
            &mut db,
            "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz, a i32, arr i32[])",
        )
        .unwrap();
        let tz = execute(&mut db, "CREATE INDEX ON t (a) WHERE ts::date IS NULL").unwrap_err();
        assert_eq!(tz.code(), "42P17", "timestamptz predicate rejected");
        let nb = execute(&mut db, "CREATE INDEX ON t (a) WHERE a").unwrap_err();
        assert_eq!(nb.code(), "42804", "non-boolean predicate rejected");
        let gin = execute(&mut db, "CREATE INDEX ON t USING gin (arr) WHERE a > 0").unwrap_err();
        assert_eq!(gin.code(), "0A000", "partial gin index rejected");
    }

    // jed_indexes surfaces a partial index's predicate canonical text; NULL for a non-partial index.
    #[test]
    fn introspection_shows_predicate() {
        let mut db = Engine::new();
        execute(
            &mut db,
            "CREATE TABLE t (id i32 PRIMARY KEY, s text, a i32)",
        )
        .unwrap();
        execute(&mut db, "CREATE INDEX ipart ON t (a) WHERE s = 'x'").unwrap();
        execute(&mut db, "CREATE INDEX ifull ON t (a)").unwrap();
        let r = rows(
            &mut db,
            "SELECT predicate FROM jed_indexes WHERE name = 'ipart'",
        );
        assert_eq!(r.len(), 1);
        let s = format!("{:?}", r[0][0]);
        assert!(s.contains('x'), "predicate should carry the text: {s}");
        let f = rows(
            &mut db,
            "SELECT predicate FROM jed_indexes WHERE name = 'ifull'",
        );
        assert!(
            matches!(f[0][0], crate::Value::Null),
            "a non-partial index has NULL predicate"
        );
    }
}

/// The canonical expression text of an owned sequence's generated `DEFAULT nextval('<seq>')`
/// (space-joined tokens, constraints.md §2). The sequence name is written by grammar.md §3's
/// quoting rule inside the string — bare when a plain lowercase identifier, else `"…"` — so the
/// regclass-style text parse resolves it exactly; the string literal then doubles any `'`.
pub(crate) fn nextval_default_text(seqname: &str) -> String {
    format!(
        "nextval ( '{}' )",
        crate::parser::quote_identifier(seqname).replace('\'', "''")
    )
}

/// Whether a column DEFAULT is the bare `NULL` literal — the one DEFAULT a composite-, array-, or
/// range-typed column admits: it is the same as no default (PostgreSQL's `AddRelationNewConstraints`
/// stores no default for a NULL constant), so it records nothing (composite.md §12).
fn is_bare_null_default(def: &DefaultDef) -> bool {
    matches!(def.expr, Expr::Literal(Literal::Null, _))
}
