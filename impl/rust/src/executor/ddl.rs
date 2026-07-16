//! Statement dispatch and DDL execution (mirrors impl/go ddl.go): the post-privilege traffic cop
//! (dispatch_stmt/dispatch_stmt_body) and the DDL executors — CREATE/DROP TABLE (with CHECK/DEFAULT/
//! serial resolution), CREATE/DROP INDEX, CREATE/DROP TYPE, CREATE/DROP/ALTER SEQUENCE — as Engine methods.

use super::*;

fn constraint_name_taken(t: &Table, name: &str) -> bool {
    t.checks.iter().any(|c| c.name.eq_ignore_ascii_case(name))
        || t.indexes
            .iter()
            .any(|i| i.unique && i.name.eq_ignore_ascii_case(name))
        || t.foreign_keys
            .iter()
            .any(|f| f.name.eq_ignore_ascii_case(name))
        || t.exclusions
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case(name))
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
        if !method.eq_ignore_ascii_case("gist") {
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
            .position(|c| c.name.eq_ignore_ascii_case(column_name))
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
        match stmt {
            Statement::Analyze(analyze) => {
                reject_params_for_ddl(params)?;
                self.execute_analyze(analyze)
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
            Statement::Explain { analyze, inner } => self.execute_explain(analyze, *inner, params),
            // Transaction control is handled by `execute_stmt_params` before dispatch.
            Statement::Begin { .. } | Statement::Commit | Statement::Rollback => {
                unreachable!("transaction control is handled before dispatch")
            }
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
        // A session-local temporary table (spec/design/temp-tables.md) is built exactly like a
        // persistent one but registered into the session temp snapshot at the end (§2), so it makes
        // zero file writes. FOREIGN KEY on a temp table is deferred this slice (§8) — rejected HERE,
        // before any persistent parent resolves, so the error is a clean 0A000 (not a 42P01 from
        // resolving a parent). The other temp narrowings (composite/collated columns, serial/IDENTITY)
        // are checked just before registration, once the columns are built.
        // Resolve the optional database qualifier (attached-databases.md §3, Slice 1b): `main`/`temp`
        // fold into the implicit scope (main = bare persistent, temp = TEMP); a host-attached name routes
        // the new table INTO that attachment's working snapshot (§6). TEMP with an explicit database is
        // contradictory unless the database IS `temp` (42601).
        let mut target_temp = ct.temp;
        let mut attach_name: Option<String> = None;
        if let Some(qual) = &ct.db {
            match qual.to_ascii_lowercase().as_str() {
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
        if target_temp && !ct.excludes.is_empty() {
            // An EXCLUDE constraint's backing GiST index would live on the temp snapshot — deferred
            // with the rest of the GiST-on-temp narrowing (spec/design/gist.md §11), a clean 0A000
            // before any column resolves.
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "an EXCLUDE constraint on a temporary table is not yet supported",
            ));
        }
        if target_temp && !ct.foreign_keys.is_empty() {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "FOREIGN KEY on a temporary table is not yet supported",
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
        check_reserved_name("table", &ct.name)?;
        // The relation namespace is shared between tables and indexes (indexes.md §2), so a
        // CREATE TABLE colliding with either kind is the same 42P07 — PG's "relation" word. For a
        // bare/main/temp target `relation_exists` is temp-aware (preclude-overlaps — temp-tables.md §3);
        // an attachment target checks its OWN snapshot's namespace (each attached database is
        // independent, §3).
        if let Some(name) = &attach_name {
            let as_snap = self
                .attach_read_snap(name)
                .expect("attachment resolved above");
            if as_snap.table(&ct.name).is_some() || as_snap.find_index(&ct.name).is_some() {
                return Err(EngineError::new(
                    SqlState::DuplicateTable,
                    format!("relation already exists: {}", ct.name),
                ));
            }
        } else if self.relation_exists(&ct.name) {
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation already exists: {}", ct.name),
            ));
        }

        let mut columns = Vec::with_capacity(ct.columns.len());
        // The primary-key member ordinals in KEY order (constraints.md §3): the column-level
        // form is the one-member case; the table-level list below records its own order.
        let mut pk: Vec<usize> = Vec::new();
        let mut pk_seen = false;
        // The OWNED sequences a `serial` column desugars to (spec/design/sequences.md §12), collected
        // during the column walk and staged into the working snapshot only after the whole CREATE
        // TABLE validates — so a later failure (e.g. a bad CHECK) discards them with the statement.
        let mut pending_serials: Vec<SequenceDef> = Vec::new();
        for def in &ct.columns {
            if columns
                .iter()
                .any(|c: &Column| c.name.eq_ignore_ascii_case(&def.name))
            {
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
                                ));
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
                    let (s, d, vlen) = resolve_type_and_typmod(&def.type_name, &def.type_mod)?;
                    // `jsonpath` is literal-only this slice (P1a) — a jsonpath COLUMN is `0A000`, like a
                    // J0-stage json column (a storable jsonpath is a follow-on).
                    if s == ScalarType::JsonPath {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "a jsonpath column is not supported yet".to_string(),
                        ));
                    }
                    (Type::Scalar(s), d, vlen)
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
                    ));
                };
            if def.primary_key {
                // The key-encodable scalars may be a PRIMARY KEY. The fixed-width ones — integers,
                // boolean (`bool-byte` §2.9), uuid (`uuid-raw16` §2.7), timestamp/timestamptz (i64
                // `int-be-signflip`, spec/design/timestamp.md §6), date (i32, spec/design/date.md §5),
                // interval (`interval-span-i128` — the 16-byte span key, encoding.md §2.10) — plus
                // the variable-width `text`/`bytea` (`…-terminated-escape`, encoding.md §2.4/§2.6) and
                // `decimal` (`decimal-order-preserving` §2.5), all self-delimiting so they compose in
                // composite keys / index suffixes — plus `float` (`float-order-preserving` §2.8 — the
                // last scalar to become keyable, so EVERY scalar is now keyable; a float at rest is
                // in-contract, determinism.md §4) — plus the `range` container (`range-bounds` §2.11,
                // the first container key) and the `array` container (`array-elements-terminated`
                // §2.14, the second container key — keyable when its element is a key-encodable scalar,
                // INCLUDING a `float` element, `is_array_keyable`). Still rejected `0A000`: only a
                // composite-element array and the recursive composite container. An oversized
                // text/bytea/decimal/range/array key (one that can't fit a node) trips the existing
                // RECORD_MAX oversized-item 0A000, mirroring PG's btree key-size limit.
                if !ty.is_integer()
                    && !ty.is_bool()
                    && !ty.is_text()
                    && !ty.is_bytea()
                    && !ty.is_decimal()
                    && !ty.is_uuid()
                    && !ty.is_timestamp()
                    && !ty.is_timestamptz()
                    && !ty.is_date()
                    && !ty.is_interval()
                    && !ty.is_float()
                    && !ty.is_range()
                    && !is_array_keyable(&ty)
                {
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
                let seqname =
                    self.choose_serial_seq_name(&ct.name, &def.name, &pending_serials, &[]);
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
                // seqname is a lowercased identifier-derived name, so the quoting is always safe.
                let expr_text = format!("nextval ( '{}' )", seqname.replace('\'', "''"));
                let expr = crate::parser::parse_expression(&expr_text)?;
                let identity_kind = def.identity.as_ref().map(|id| {
                    if id.always {
                        IdentityKind::Always
                    } else {
                        IdentityKind::ByDefault
                    }
                });
                (None, Some(DefaultExpr { expr_text, expr }), identity_kind)
            } else if ty.is_composite() || ty.is_array() || ty.is_range() {
                // A DEFAULT on a composite-, array-, or range-typed column is not supported this
                // slice (composite.md §12 / array.md §12 / ranges.md §8).
                if def.default.is_some() {
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
                        if matches!(&d.expr, Expr::Literal(Literal::Text(s))
                            if crate::date::date_clock_is_relative(s)));
                match &def.default {
                    None => (None, None, None),
                    Some(d) => match &d.expr {
                        Expr::Literal(lit) if !clock_default => (
                            Some(store_value(
                                literal_to_value_for(lit, sty)?,
                                sty,
                                decimal,
                                varchar_len,
                                false,
                                &def.name,
                            )?),
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
                                return Err(type_error(format!(
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
                                }),
                                None,
                            )
                        }
                    },
                }
            };
            // The column's effective collation, frozen now (spec/design/collation.md §1). An explicit
            // `COLLATE "name"` is text-only (42804 otherwise, PG-matching) and must name a loaded
            // collation or `C` (42704); a text column without a clause inherits the per-database
            // default. A `C` effective collation stores as `None` (the fast path).
            let collation: Option<String> = if let Some(name) = &def.collation {
                if !ty.is_text() {
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
            } else if ty.is_text() {
                self.read_snap().default_collation().map(str::to_string)
            } else {
                None
            };
            columns.push(Column {
                name: def.name.clone(),
                ty,
                decimal,
                varchar_len,
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
                    .position(|c: &Column| c.name.eq_ignore_ascii_case(name))
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
                if !ty.is_integer()
                    && !ty.is_bool()
                    && !ty.is_text()
                    && !ty.is_bytea()
                    && !ty.is_decimal()
                    && !ty.is_uuid()
                    && !ty.is_timestamp()
                    && !ty.is_timestamptz()
                    && !ty.is_date()
                    && !ty.is_interval()
                    && !ty.is_float()
                    && !ty.is_range()
                    && !is_array_keyable(ty)
                {
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
                    .position(|c: &Column| c.name.eq_ignore_ascii_case(cname))
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
                if !ty.is_integer()
                    && !ty.is_bool()
                    && !ty.is_text()
                    && !ty.is_bytea()
                    && !ty.is_decimal()
                    && !ty.is_uuid()
                    && !ty.is_timestamp()
                    && !ty.is_timestamptz()
                    && !ty.is_date()
                    && !ty.is_interval()
                    && !ty.is_float()
                    && !ty.is_range()
                    && !is_array_keyable(ty)
                {
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
        };
        for def in &ct.checks {
            // Structural rejections first (a single pre-walk — a documented micro-order
            // divergence from PG, which interleaves them with name/type resolution):
            // subquery 0A000, aggregate 42803, bind parameter 42P02 (constraints.md §4.1).
            reject_check_structure(&def.expr)?;
            let scope = Scope::single(self, &table);
            let (_, ty) = resolve(
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
                    return Err(type_error("argument of CHECK must be boolean"));
                }
            }
        }
        // Naming (constraints.md §4.3): a single pass in textual order. An explicit name is
        // used as written; a derived name is built from the LOWERCASED table/column names —
        // `<table>_<col>_check` when the expression references exactly one distinct column,
        // else `<table>_check` — suffixed with the smallest positive integer that frees it.
        // A collision (case-insensitive, PG folds) is 42710; derived names never yield to a
        // later explicit one (oracle-probed).
        let mut checks: Vec<CheckConstraint> = Vec::with_capacity(ct.checks.len());
        for def in &ct.checks {
            let name = match &def.name {
                Some(n) => {
                    if checks.iter().any(|c| c.name.eq_ignore_ascii_case(n)) {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n.clone()
                }
                None => {
                    let cols = check_referenced_columns(&def.expr, &table.columns);
                    let base = match cols.as_slice() {
                        [i] => format!(
                            "{}_{}_check",
                            table.name.to_ascii_lowercase(),
                            table.columns[*i].name.to_ascii_lowercase()
                        ),
                        _ => format!("{}_check", table.name.to_ascii_lowercase()),
                    };
                    let mut candidate = base.clone();
                    let mut suffix = 0u32;
                    while checks
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&candidate))
                    {
                        suffix += 1;
                        candidate = format!("{base}{suffix}");
                    }
                    candidate
                }
            };
            checks.push(CheckConstraint {
                name,
                expr_text: def.text.clone(),
                expr: def.expr.clone(),
            });
        }
        // Evaluation (and on-disk) order: ascending byte order of the lowercased name
        // (constraints.md §4.4 — PG evaluates checks sorted by name, oracle-probed).
        checks.sort_by_key(|c| c.name.to_ascii_lowercase());
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
                exec.relation_exists(n)
                    || t.name.eq_ignore_ascii_case(n)
                    || t.indexes.iter().any(|i| i.name.eq_ignore_ascii_case(n))
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
                    if table.checks.iter().any(|c| c.name.eq_ignore_ascii_case(&n)) {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n
                }
                None => {
                    let mut base = table.name.to_ascii_lowercase();
                    for &i in &cols {
                        base.push('_');
                        base.push_str(&table.columns[i].name.to_ascii_lowercase());
                    }
                    base.push_str("_key");
                    let mut candidate = base.clone();
                    let mut suffix = 0u32;
                    while taken(self, &table, &candidate)
                        || table
                            .checks
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&candidate))
                    {
                        suffix += 1;
                        candidate = format!("{base}{suffix}");
                    }
                    candidate
                }
            };
            // Insert in catalog (ascending lowercased-name) order — indexes.md §6.
            let name_key = name.to_ascii_lowercase();
            let pos = table
                .indexes
                .iter()
                .position(|i| i.name.to_ascii_lowercase() > name_key)
                .unwrap_or(table.indexes.len());
            table.indexes.insert(
                pos,
                IndexDef {
                    name,
                    keys: cols.into_iter().map(IndexKey::Column).collect(),
                    unique: true,
                    kind: IndexKind::Btree,
                    predicate: None,
                },
            );
        }

        // FOREIGN KEY constraints (constraints.md §6). Resolved AFTER the PK / UNIQUE / CHECK
        // constraints (PG's order), each in textual definition order: resolve the local columns
        // (42703/42701) against this table; look up the parent (42P01, or the table itself for a
        // self-reference); resolve the referenced columns (default to the parent PK, 42830 if it
        // has none); check the arity (42830); name the constraint (explicit collision 42710, else
        // derive `<table>_<cols>_fkey` with a suffix walk through the constraint namespace);
        // reject the unsupported write-actions (0A000); require the referenced columns to be the
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
                    .position(|c| c.name.eq_ignore_ascii_case(cname))
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
            // 2. Parent table — a self-reference resolves against the in-progress definition. The
            // parent must be PERSISTENT (resolve against the main snapshot, not the temp-aware funnel):
            // a persistent table may not reference a temp parent, and a temp table has no FK at all
            // (rejected above) — so a temp parent reads as "does not exist" here (temp-tables.md §8).
            let self_ref = fk.ref_table.eq_ignore_ascii_case(&table.name);
            let parent: &Table = if self_ref {
                &table
            } else {
                self.read_snap().table(&fk.ref_table).ok_or_else(|| {
                    EngineError::new(
                        SqlState::UndefinedTable,
                        format!("table does not exist: {}", fk.ref_table),
                    )
                })?
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
                            .position(|c| c.name.eq_ignore_ascii_case(cname))
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
            // 5. Name — the per-table constraint namespace, shared with CHECK (§6.2/§6.7).
            let name = match &fk.name {
                Some(n) => {
                    if table.checks.iter().any(|c| c.name.eq_ignore_ascii_case(n))
                        || resolved_fks.iter().any(|f| f.name.eq_ignore_ascii_case(n))
                    {
                        return Err(EngineError::new(
                            SqlState::DuplicateObject,
                            format!("constraint {n} for relation {} already exists", table.name),
                        ));
                    }
                    n.clone()
                }
                None => {
                    let mut base = table.name.to_ascii_lowercase();
                    for &i in &local {
                        base.push('_');
                        base.push_str(&table.columns[i].name.to_ascii_lowercase());
                    }
                    base.push_str("_fkey");
                    let mut candidate = base.clone();
                    let mut suffix = 0u32;
                    while table
                        .checks
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&candidate))
                        || resolved_fks
                            .iter()
                            .any(|f| f.name.eq_ignore_ascii_case(&candidate))
                    {
                        suffix += 1;
                        candidate = format!("{base}{suffix}");
                    }
                    candidate
                }
            };
            // 6. Reject the unsupported write-actions (§6.6).
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
                on_delete,
                on_update,
            });
        }
        // Held in ascending lowercased-name order (the catalog's on-disk + evaluation order, §6.9).
        resolved_fks.sort_by_key(|f| f.name.to_ascii_lowercase());
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
                exec.relation_exists(n)
                    || t.name.eq_ignore_ascii_case(n)
                    || t.indexes.iter().any(|i| i.name.eq_ignore_ascii_case(n))
            };
            let constraint_taken = |t: &Table, n: &str| {
                t.checks.iter().any(|c| c.name.eq_ignore_ascii_case(n))
                    || t.foreign_keys
                        .iter()
                        .any(|f| f.name.eq_ignore_ascii_case(n))
                    || t.exclusions.iter().any(|e| e.name.eq_ignore_ascii_case(n))
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
                    let mut base = table.name.to_ascii_lowercase();
                    for &i in &indices {
                        base.push('_');
                        base.push_str(&table.columns[i].name.to_ascii_lowercase());
                    }
                    base.push_str("_excl");
                    let mut candidate = base.clone();
                    let mut suffix = 0u32;
                    while taken(self, &table, &candidate) || constraint_taken(&table, &candidate) {
                        suffix += 1;
                        candidate = format!("{base}{suffix}");
                    }
                    candidate
                }
            };
            // Insert the backing GiST index in catalog (ascending lowercased-name) order.
            let name_key = name.to_ascii_lowercase();
            let pos = table
                .indexes
                .iter()
                .position(|i| i.name.to_ascii_lowercase() > name_key)
                .unwrap_or(table.indexes.len());
            table.indexes.insert(
                pos,
                IndexDef {
                    name: name.clone(),
                    keys: indices.into_iter().map(IndexKey::Column).collect(),
                    unique: false,
                    kind: IndexKind::Gist,
                    predicate: None,
                },
            );
            table.exclusions.push(ExclusionConstraint {
                name: name.clone(),
                index: name,
                elements,
            });
        }
        // Held in ascending lowercased-name order (the catalog's on-disk order — gist.md §8).
        table
            .exclusions
            .sort_by_key(|e| e.name.to_ascii_lowercase());

        let index_keys: Vec<String> = table
            .indexes
            .iter()
            .map(|i| i.name.to_ascii_lowercase())
            .collect();
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
            Some(q) => q.eq_ignore_ascii_case("temp"),
            None => self.is_temp_table(&at.name),
        };
        let snap = match at.db.as_deref().map(str::to_ascii_lowercase) {
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
        let old_key = original.name.to_ascii_lowercase();
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
            .is_some_and(|q| !matches!(q.to_ascii_lowercase().as_str(), "main" | "temp"));
        let main_namespace = self.read_snap().clone();
        let temp_namespace = self.temp_read_snap().clone();
        let relation_taken = |name: &str| {
            if attachment_scope {
                snap.table(name).is_some()
                    || snap.find_index(name).is_some()
                    || snap.sequence(name).is_some()
            } else {
                main_namespace.table(name).is_some()
                    || main_namespace.find_index(name).is_some()
                    || main_namespace.sequence(name).is_some()
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
            if pending_sequences
                .iter()
                .any(|s| s.name.eq_ignore_ascii_case(name))
            {
                return true;
            }
            if working
                .indexes
                .iter()
                .any(|i| i.name.eq_ignore_ascii_case(name))
            {
                true
            } else if original
                .indexes
                .iter()
                .any(|i| i.name.eq_ignore_ascii_case(name))
            {
                false
            } else if dropped_sequences.contains(&name.to_ascii_lowercase()) {
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
                table.name = new_name;
                rename_table = true;
            }
            AlterTableAction::RenameColumn { old, new } => {
                let Some(ci) = table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&old))
                else {
                    return Err(EngineError::new(
                        SqlState::UndefinedColumn,
                        format!("column does not exist: {old}"),
                    ));
                };
                if table
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&new))
                {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column already exists: {new}"),
                    ));
                }
                self.rewrite_table_expressions(&mut table, Some((&old, &new)), None)?;
                table.columns[ci].name = new;
            }
            AlterTableAction::RenameConstraint { old, new } => {
                let exists = |name: &str, t: &Table| {
                    t.checks.iter().any(|c| c.name.eq_ignore_ascii_case(name))
                        || t.indexes
                            .iter()
                            .any(|i| i.unique && i.name.eq_ignore_ascii_case(name))
                        || t.foreign_keys
                            .iter()
                            .any(|f| f.name.eq_ignore_ascii_case(name))
                        || t.exclusions
                            .iter()
                            .any(|e| e.name.eq_ignore_ascii_case(name))
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
                    if c.name.eq_ignore_ascii_case(&old) {
                        c.name = new.clone();
                    }
                }
                for f in &mut table.foreign_keys {
                    if f.name.eq_ignore_ascii_case(&old) {
                        f.name = new.clone();
                    }
                }
                for e in &mut table.exclusions {
                    if e.name.eq_ignore_ascii_case(&old) {
                        e.name = new.clone();
                        e.index = new.clone();
                        index_rename = Some((old.clone(), new.clone()));
                    }
                }
                for i in &mut table.indexes {
                    if (i.unique || index_rename.is_some()) && i.name.eq_ignore_ascii_case(&old) {
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
                table.checks.sort_by_key(|x| x.name.to_ascii_lowercase());
                table.indexes.sort_by_key(|x| x.name.to_ascii_lowercase());
                table
                    .foreign_keys
                    .sort_by_key(|x| x.name.to_ascii_lowercase());
                table
                    .exclusions
                    .sort_by_key(|x| x.name.to_ascii_lowercase());
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
                        if table
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&column.name))
                        {
                            if if_not_exists {
                                continue;
                            }
                            return Err(EngineError::new(
                                SqlState::DuplicateColumn,
                                format!("column already exists: {}", column.name),
                            ));
                        }
                        if (!foreign_keys.is_empty() && temp_scope)
                            || (!foreign_keys.is_empty() && attachment_scope)
                        {
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
                            })?
                            .pop()
                            .unwrap();
                        row_steps.push(RowStep::Add {
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
                        let Some(ci) = table
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(&name))
                        else {
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
                        let dropped_source = column_sources[ci].clone();
                        // PostgreSQL owns an FK through its local (referencing) columns. Dropping
                        // one removes the whole FK under RESTRICT. A self-FK that uses the column
                        // only on the referenced side remains a dependency.
                        let table_name = table.name.clone();
                        let fk_uses_local_column =
                            |fk: &ForeignKeyConstraint| fk.columns.contains(&ci);
                        let fk_uses_self_ref_column = |fk: &ForeignKeyConstraint| {
                            fk.ref_table.eq_ignore_ascii_case(&table_name)
                                && fk.ref_columns.contains(&ci)
                        };
                        let self_ref_fk_dep = table
                            .foreign_keys
                            .iter()
                            .any(|fk| fk_uses_self_ref_column(fk) && !fk_uses_local_column(fk));
                        let incoming_dep = match column_sources[ci] {
                            ColumnSource::Original(old_ci) => snap.tables.iter().any(|(key, t)| {
                                !key.eq_ignore_ascii_case(&old_key)
                                    && t.foreign_keys.iter().any(|fk| {
                                        fk.ref_table.eq_ignore_ascii_case(&original.name)
                                            && fk.ref_columns.contains(&old_ci)
                                            && !cascade_dropped_column_fks.contains(&(
                                                key.to_ascii_lowercase(),
                                                fk.name.to_ascii_lowercase(),
                                            ))
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
                            ));
                        }
                        if cascade {
                            if let ColumnSource::Original(old_ci) = column_sources[ci] {
                                for (key, child) in snap.tables.iter() {
                                    if key.eq_ignore_ascii_case(&old_key) {
                                        continue;
                                    }
                                    for fk in &child.foreign_keys {
                                        if fk.ref_table.eq_ignore_ascii_case(&original.name)
                                            && fk.ref_columns.contains(&old_ci)
                                        {
                                            cascade_dropped_column_fks.insert((
                                                key.to_ascii_lowercase(),
                                                fk.name.to_ascii_lowercase(),
                                            ));
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
                                removed.push(c.name.to_ascii_lowercase());
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
                                removed.push(ix.name.to_ascii_lowercase());
                            }
                            !uses
                        });
                        table.exclusions.retain(|e| {
                            let uses = e.elements.iter().any(|x| x.column == ci);
                            if uses {
                                removed.push(e.name.to_ascii_lowercase());
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
                                removed_fks.push(fk.name.to_ascii_lowercase());
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
                            if fk.ref_table.eq_ignore_ascii_case(&table.name) {
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
                            if !owner.table.eq_ignore_ascii_case(&table.name) {
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
                            let Some(ci) = table
                                .columns
                                .iter()
                                .position(|c| c.name.eq_ignore_ascii_case(&name))
                            else {
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
                            if ty.is_composite()
                                || (ty.is_array() && !is_array_keyable(ty))
                                || (!ty.is_array() && !ty.is_range() && !is_keyable_scalar(ty))
                            {
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
                            fk.ref_table.eq_ignore_ascii_case(&table.name)
                                && sorted_unique(&fk.ref_columns) == old_pk
                        });
                        let incoming_deps = snap.tables.iter().any(|(key, child)| {
                            !key.eq_ignore_ascii_case(&old_key)
                                && child.foreign_keys.iter().any(|fk| {
                                    fk.ref_table.eq_ignore_ascii_case(&original.name)
                                        && sorted_unique(&fk.ref_columns) == old_pk
                                })
                        });
                        if !cascade && (local_deps || incoming_deps) {
                            return Err(EngineError::new(
                                SqlState::DependentObjectsStillExist,
                                "cannot drop primary key because other objects depend on it"
                                    .to_string(),
                            ));
                        }
                        if cascade {
                            table.foreign_keys.retain(|fk| {
                                !(fk.ref_table.eq_ignore_ascii_case(&table.name)
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
                            if temp_scope {
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
                                let (_, ty) = resolve(
                                    &Scope::single(self, &table),
                                    &d.expr,
                                    None,
                                    &mut AggCtx::Forbidden,
                                    &mut ParamTypes::default(),
                                )?;
                                if !matches!(ty, ResolvedType::Bool | ResolvedType::Null) {
                                    return Err(type_error("argument of CHECK must be boolean"));
                                }
                                let mut name = d.name.clone().unwrap_or_else(|| {
                                    format!("{}_check", table.name.to_ascii_lowercase())
                                });
                                if d.name.is_none() {
                                    let rs = check_referenced_columns(&d.expr, &table.columns);
                                    if rs.len() == 1 {
                                        name = format!(
                                            "{}_{}_check",
                                            table.name.to_ascii_lowercase(),
                                            table.columns[rs[0]].name.to_ascii_lowercase()
                                        );
                                    }
                                    let base = name.clone();
                                    let mut suffix = 0u32;
                                    while constraint_name_taken(&table, &name) {
                                        suffix += 1;
                                        name = format!("{base}{suffix}");
                                    }
                                } else if constraint_name_taken(&table, &name) {
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
                                });
                                table.checks.sort_by_key(|x| x.name.to_ascii_lowercase());
                                added_constraints.insert(name.to_ascii_lowercase());
                            }
                            AlterConstraintDef::Unique(d) => {
                                let mut cols = Vec::new();
                                for n in d.columns {
                                    let Some(ci) = table
                                        .columns
                                        .iter()
                                        .position(|c| c.name.eq_ignore_ascii_case(&n))
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
                                    if !ty.is_integer()
                                        && !ty.is_bool()
                                        && !ty.is_text()
                                        && !ty.is_bytea()
                                        && !ty.is_decimal()
                                        && !ty.is_uuid()
                                        && !ty.is_timestamp()
                                        && !ty.is_timestamptz()
                                        && !ty.is_date()
                                        && !ty.is_interval()
                                        && !ty.is_float()
                                        && !ty.is_range()
                                        && !is_array_keyable(ty)
                                    {
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
                                    let base = format!(
                                        "{}_{}_key",
                                        table.name.to_ascii_lowercase(),
                                        cols.iter()
                                            .map(|i| table.columns[*i].name.to_ascii_lowercase())
                                            .collect::<Vec<_>>()
                                            .join("_")
                                    );
                                    let mut name = base.clone();
                                    let mut suffix = 0u32;
                                    while working_relation_taken(
                                        &table,
                                        &pending_serials,
                                        &dropped_owned_sequences,
                                        &name,
                                    ) || constraint_name_taken(&table, &name)
                                    {
                                        suffix += 1;
                                        name = format!("{base}{suffix}");
                                    }
                                    name
                                };
                                table.indexes.push(IndexDef {
                                    name: name.clone(),
                                    keys: cols.into_iter().map(IndexKey::Column).collect(),
                                    unique: true,
                                    kind: IndexKind::Btree,
                                    predicate: None,
                                });
                                table.indexes.sort_by_key(|x| x.name.to_ascii_lowercase());
                                added_constraints.insert(name.to_ascii_lowercase());
                            }
                            AlterConstraintDef::ForeignKey(d) => {
                                let mut local = Vec::new();
                                for n in d.columns {
                                    let Some(ci) = table
                                        .columns
                                        .iter()
                                        .position(|c| c.name.eq_ignore_ascii_case(&n))
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
                                let parent = if d.ref_table.eq_ignore_ascii_case(&table.name) {
                                    table.clone()
                                } else {
                                    snap.table(&d.ref_table).cloned().ok_or_else(|| {
                                        EngineError::new(
                                            SqlState::UndefinedTable,
                                            format!("table does not exist: {}", d.ref_table),
                                        )
                                    })?
                                };
                                let refs = if let Some(ns) = d.ref_columns {
                                    let mut r = Vec::new();
                                    for n in ns {
                                        let Some(i) = parent
                                            .columns
                                            .iter()
                                            .position(|c| c.name.eq_ignore_ascii_case(&n))
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
                                    let base = format!(
                                        "{}_{}_fkey",
                                        table.name.to_ascii_lowercase(),
                                        local
                                            .iter()
                                            .map(|i| table.columns[*i].name.to_ascii_lowercase())
                                            .collect::<Vec<_>>()
                                            .join("_")
                                    );
                                    let mut name = base.clone();
                                    let mut suffix = 0u32;
                                    while constraint_name_taken(&table, &name) {
                                        suffix += 1;
                                        name = format!("{base}{suffix}");
                                    }
                                    name
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
                                    on_delete: fk_action(d.on_delete, "DELETE")?,
                                    on_update: fk_action(d.on_update, "UPDATE")?,
                                });
                                table
                                    .foreign_keys
                                    .sort_by_key(|x| x.name.to_ascii_lowercase());
                                added_constraints.insert(name.to_ascii_lowercase());
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
                                    let base = format!(
                                        "{}_{}_excl",
                                        table.name.to_ascii_lowercase(),
                                        cols.iter()
                                            .map(|i| table.columns[*i].name.to_ascii_lowercase())
                                            .collect::<Vec<_>>()
                                            .join("_")
                                    );
                                    let mut name = base.clone();
                                    let mut suffix = 0u32;
                                    while working_relation_taken(
                                        &table,
                                        &pending_serials,
                                        &dropped_owned_sequences,
                                        &name,
                                    ) || constraint_name_taken(&table, &name)
                                    {
                                        suffix += 1;
                                        name = format!("{base}{suffix}");
                                    }
                                    name
                                };
                                table.indexes.push(IndexDef {
                                    name: name.clone(),
                                    keys: cols.into_iter().map(IndexKey::Column).collect(),
                                    unique: false,
                                    kind: IndexKind::Gist,
                                    predicate: None,
                                });
                                table.exclusions.push(ExclusionConstraint {
                                    name: name.clone(),
                                    index: name.clone(),
                                    elements: els,
                                });
                                table.indexes.sort_by_key(|x| x.name.to_ascii_lowercase());
                                table
                                    .exclusions
                                    .sort_by_key(|x| x.name.to_ascii_lowercase());
                                added_constraints.insert(name.to_ascii_lowercase());
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
                        let k = name.to_ascii_lowercase();
                        let mut found = false;
                        let n = table.checks.len();
                        table.checks.retain(|x| !x.name.eq_ignore_ascii_case(&name));
                        found |= n != table.checks.len();
                        let n = table.foreign_keys.len();
                        table
                            .foreign_keys
                            .retain(|x| !x.name.eq_ignore_ascii_case(&name));
                        found |= n != table.foreign_keys.len();
                        if let Some(e) = table
                            .exclusions
                            .iter()
                            .find(|x| x.name.eq_ignore_ascii_case(&name))
                            .cloned()
                        {
                            table
                                .exclusions
                                .retain(|x| !x.name.eq_ignore_ascii_case(&name));
                            table
                                .indexes
                                .retain(|x| !x.name.eq_ignore_ascii_case(&e.index));
                            found = true;
                        }
                        if let Some(ix) = table
                            .indexes
                            .iter()
                            .find(|x| x.unique && x.name.eq_ignore_ascii_case(&name))
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
                                    f.ref_table.eq_ignore_ascii_case(&table.name)
                                        && sorted_unique(&f.ref_columns) == cols
                                })
                                .count();
                            let mut external_deps = 0usize;
                            if let Some(old_cols) = &original_cols {
                                for (key, child) in snap.tables.iter() {
                                    if key.eq_ignore_ascii_case(&old_key) {
                                        continue;
                                    }
                                    for fk in &child.foreign_keys {
                                        let fk_cols = sorted_unique(&fk.ref_columns);
                                        if fk.ref_table.eq_ignore_ascii_case(&table.name)
                                            && fk_cols == *old_cols
                                            && !cascade_dropped_column_fks.contains(&(
                                                key.to_ascii_lowercase(),
                                                fk.name.to_ascii_lowercase(),
                                            ))
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
                                ));
                            }
                            if cascade {
                                if let Some(old_cols) = original_cols {
                                    cascade_unique_cols.push(old_cols);
                                }
                                table.foreign_keys.retain(|f| {
                                    !(f.ref_table.eq_ignore_ascii_case(&table.name)
                                        && sorted_unique(&f.ref_columns) == cols)
                                });
                            }
                            table
                                .indexes
                                .retain(|x| !x.name.eq_ignore_ascii_case(&name));
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
                    let Some(ci) = table
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(&edit.column))
                    else {
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
                            let old_column = table.columns[ci].clone();
                            let mut target = self.build_alter_added_column(
                                &table,
                                &crate::ast::ColumnDef {
                                    name: table.columns[ci].name.clone(),
                                    type_name: type_name.clone(),
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
                            if old_column.primary_key
                                && (target.ty.is_composite()
                                    || (target.ty.is_array() && !is_array_keyable(&target.ty))
                                    || (!target.ty.is_array()
                                        && !target.ty.is_range()
                                        && !is_keyable_scalar(&target.ty)))
                            {
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
                            let source = using
                                .unwrap_or_else(|| Expr::Column(table.columns[ci].name.clone()));
                            let cast = Expr::Cast {
                                inner: Box::new(source),
                                type_name: type_name.clone(),
                                type_mod,
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
                                };
                                let default_cast = Expr::Cast {
                                    inner: Box::new(Expr::Column(old_column.name.clone())),
                                    type_name: type_name.clone(),
                                    type_mod,
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
                                };
                                let mut default_meter = self.session.new_meter();
                                target.default =
                                    Some(node.eval(&[default], &env, &mut default_meter)?);
                                prep_cost += default_meter.accrued;
                            } else if let Some(default) = old_column.default_expr.clone() {
                                let default_cast = Expr::Cast {
                                    inner: Box::new(default.expr),
                                    type_name: type_name.clone(),
                                    type_mod,
                                };
                                resolve(
                                    &Scope::empty(self),
                                    &default_cast,
                                    None,
                                    &mut AggCtx::Forbidden,
                                    &mut ParamTypes::default(),
                                )?;
                                let type_sql = match type_mod {
                                    Some(m) => match m.scale {
                                        Some(s) => format!("{type_name} ({}, {})", m.precision, s),
                                        None => format!("{type_name} ({})", m.precision),
                                    },
                                    None => type_name.clone(),
                                };
                                target.default_expr = Some(DefaultExpr {
                                    expr_text: format!(
                                        "CAST ( {} AS {} )",
                                        default.expr_text, type_sql
                                    ),
                                    expr: default_cast,
                                });
                            }
                            if old_column.identity.is_some() {
                                let old_dtype = SeqDataType::for_scalar(old_column.ty.scalar())
                                    .expect("identity column is integer");
                                let new_dtype = SeqDataType::for_scalar(target.ty.scalar())
                                    .expect("identity column is integer");
                                if let Some(si) = pending_serials.iter().position(|seq| {
                                    seq.owned_by.as_ref().is_some_and(|owner| {
                                        owner.table.eq_ignore_ascii_case(&table.name)
                                            && usize::from(owner.column) == ci
                                    })
                                }) {
                                    pending_serials[si] = retype_identity_sequence(
                                        &pending_serials[si],
                                        old_dtype,
                                        new_dtype,
                                    )?;
                                } else {
                                    let old_ci = match column_sources[ci] {
                                        ColumnSource::Original(old) => old,
                                        ColumnSource::Added => {
                                            panic!("added identity column has pending sequence")
                                        }
                                    };
                                    let key = snap
                                        .sequences_owned_by(&original.name)
                                        .into_iter()
                                        .find(|key| {
                                            snap.sequence(key)
                                                .and_then(|seq| seq.owned_by.as_ref())
                                                .is_some_and(|owner| {
                                                    usize::from(owner.column) == old_ci
                                                })
                                        })
                                        .expect("identity column has owned sequence");
                                    let current = self
                                        .sequence(&key)
                                        .cloned()
                                        .expect("identity owned sequence exists");
                                    self.put_sequence_routed(retype_identity_sequence(
                                        &current, old_dtype, new_dtype,
                                    )?);
                                }
                            }
                            table.columns[ci] = target;
                            type_changed_columns[ci] = true;
                            type_changed = true;
                            row_steps.push(RowStep::SetType { column: ci, value });
                            for c in &table.checks {
                                added_constraints.insert(c.name.to_ascii_lowercase());
                            }
                            for i in &table.indexes {
                                added_constraints.insert(i.name.to_ascii_lowercase());
                            }
                            for f in &table.foreign_keys {
                                added_constraints.insert(f.name.to_ascii_lowercase());
                            }
                            for e in &table.exclusions {
                                added_constraints.insert(e.name.to_ascii_lowercase());
                            }
                        }
                    }
                }
            }
        }

        if type_changed {
            for fk in &table.foreign_keys {
                let parent = if fk.ref_table.eq_ignore_ascii_case(&table.name) {
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
                if key.eq_ignore_ascii_case(&old_key) {
                    continue;
                }
                for fk in &child.foreign_keys {
                    if !fk.ref_table.eq_ignore_ascii_case(&original.name) {
                        continue;
                    }
                    if cascade_dropped_column_fks
                        .contains(&(key.to_ascii_lowercase(), fk.name.to_ascii_lowercase()))
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

        let page_size = at
            .db
            .as_deref()
            .filter(|q| !matches!(q.to_ascii_lowercase().as_str(), "main" | "temp"))
            .map(|q| self.attach_page_size(&q.to_ascii_lowercase()))
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
        if table_rewrite_needed {
            // Make owned sequences visible to their synthesized nextval defaults. Statement-level
            // rollback restores the prior working root if any later validation fails.
            for seq in pending_serials.iter().cloned() {
                match at.db.as_deref().map(str::to_ascii_lowercase) {
                    None if temp_scope => self.temp_working_mut().put_sequence(seq),
                    None => self.working_mut().put_sequence(seq),
                    Some(q) if q == "temp" => self.temp_working_mut().put_sequence(seq),
                    Some(q) if q == "main" => self.working_mut().put_sequence(seq),
                    Some(q) => self.attach_write_snap(&q).put_sequence(seq),
                }
            }
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
            };
            let pk: Vec<(usize, Type)> = table
                .pk
                .iter()
                .map(|&i| (i, table.columns[i].ty.clone()))
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
                        RowStep::Add { column, default } => {
                            let value =
                                self.eval_default(column, default.as_ref(), &rng, &mut meter)?;
                            row.push(value);
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
                        *key = encode_pk_key(&pk, &colls, row)?;
                    }
                    if !seen_keys.insert(key.clone()) {
                        return Err(EngineError::unique_violation(
                            &table.name,
                            format!("{}_pkey", table.name.to_ascii_lowercase()),
                        ));
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
                .filter(|ix| added_constraints.contains(&ix.name.to_ascii_lowercase()))
            {
                constraint_entries.insert(ix.name.to_ascii_lowercase(), Vec::new());
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
            let checks = self.resolve_checks(&table)?;
            let colls = self.column_collations(&table.columns);
            let mut seen =
                std::collections::HashMap::<String, std::collections::HashSet<Vec<u8>>>::new();
            let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
            for (key, mut row) in rows.iter().cloned() {
                meter.guard()?;
                if rewrite_entries.is_none() {
                    meter.charge(COSTS.storage_row_read);
                }
                store.resolve_inline_columns(&mut row)?;
                for ck in checks
                    .iter()
                    .filter(|(n, _)| added_constraints.contains(&n.to_ascii_lowercase()))
                {
                    self.eval_checks(
                        std::slice::from_ref(ck),
                        &row,
                        &rng,
                        &table.name,
                        &mut meter,
                    )?;
                }
                for ix in table
                    .indexes
                    .iter()
                    .filter(|i| added_constraints.contains(&i.name.to_ascii_lowercase()))
                {
                    let ri = self.resolve_index(&table, ix)?;
                    if ix.unique {
                        if let Some(p) = self.index_prefix(&table.columns, &colls, &ri, &row)? {
                            let s = seen.entry(ix.name.to_ascii_lowercase()).or_default();
                            if !s.insert(p) {
                                return Err(EngineError::unique_violation(&table.name, &ix.name));
                            }
                        }
                    }
                    constraint_entries
                        .entry(ix.name.to_ascii_lowercase())
                        .or_default()
                        .extend(self.index_entries(&table.columns, &colls, &ri, &key, &row)?);
                }
            }
            for fk in table
                .foreign_keys
                .iter()
                .filter(|f| added_constraints.contains(&f.name.to_ascii_lowercase()))
            {
                let parent = if fk.ref_table.eq_ignore_ascii_case(&table.name) {
                    &table
                } else {
                    snap.table(&fk.ref_table).unwrap()
                };
                let pc = self.column_collations(&parent.columns);
                for (_, mut row) in rows.iter().cloned() {
                    store.resolve_inline_columns(&mut row)?;
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
                            self.fk_probe_hits(&p, &parent.name)?
                        };
                        if !hit {
                            return Err(EngineError::fk_violation_insert(&table.name, &fk.name));
                        }
                    }
                }
            }
            for ex in table
                .exclusions
                .iter()
                .filter(|e| added_constraints.contains(&e.name.to_ascii_lowercase()))
            {
                for i in 0..rows.len() {
                    let mut a = rows[i].1.clone();
                    store.resolve_inline_columns(&mut a)?;
                    for j in 0..i {
                        meter.charge(COSTS.constraint_check);
                        meter.guard()?;
                        let mut b = rows[j].1.clone();
                        store.resolve_inline_columns(&mut b)?;
                        if exclusion_pair_conflicts(&table.columns, ex, &a, &b) {
                            return Err(EngineError::exclusion_violation(&table.name, &ex.name));
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
            for ix in &table.indexes {
                let ri = self.resolve_index(&table, ix)?;
                let out = constraint_entries
                    .entry(ix.name.to_ascii_lowercase())
                    .or_default();
                for (key, row) in rewrite_entries.as_ref().unwrap() {
                    out.extend(self.index_entries(&table.columns, &colls, &ri, key, row)?);
                }
                out.sort();
            }
        }
        let ir = index_rename.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
        let ws = match at.db.as_deref().map(str::to_ascii_lowercase) {
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
            let pending: std::collections::HashSet<String> = pending_serials
                .iter()
                .map(|s| s.name.to_ascii_lowercase())
                .collect();
            ws.remap_alter_column_dependents(&table.name, &original_to_new, &pending);
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
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a DEFAULT on this container type is not supported yet".to_string(),
            ));
        };
        let sty = *sty;
        let clock_default = sty.is_date()
            && matches!(&def.expr, Expr::Literal(Literal::Text(s)) if crate::date::date_clock_is_relative(s));
        if let Expr::Literal(lit) = &def.expr
            && !clock_default
        {
            return Ok((
                Some(store_value(
                    literal_to_value_for(lit, sty)?,
                    sty,
                    col.decimal,
                    col.varchar_len,
                    false,
                    &col.name,
                )?),
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
            return Err(type_error(format!(
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
                            ));
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
                let (s, d, v) = resolve_type_and_typmod(&def.type_name, &def.type_mod)?;
                if s == ScalarType::JsonPath {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a jsonpath column is not supported yet",
                    ));
                }
                (Type::Scalar(s), d, v)
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
                ));
            };

        let collation = if let Some(name) = &def.collation {
            if !ty.is_text() {
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
        } else if ty.is_text() {
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
            primary_key: def.primary_key,
            not_null: def.primary_key
                || def.not_null
                || serial_kind.is_some()
                || def.identity.is_some(),
            default: None,
            default_expr: None,
            identity: None,
            collation,
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
            if !col.ty.is_integer()
                && !col.ty.is_bool()
                && !col.ty.is_text()
                && !col.ty.is_bytea()
                && !col.ty.is_decimal()
                && !col.ty.is_uuid()
                && !col.ty.is_timestamp()
                && !col.ty.is_timestamptz()
                && !col.ty.is_date()
                && !col.ty.is_interval()
                && !col.ty.is_float()
                && !col.ty.is_range()
                && !is_array_keyable(&col.ty)
            {
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
            let seqname = self.choose_serial_seq_name(
                &table.name,
                &def.name,
                pending_serials,
                reserved_relation_names,
            );
            let mut opts = def
                .identity
                .as_ref()
                .map(|x| x.options.clone())
                .unwrap_or_default();
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
            let expr_text = format!("nextval ( '{}' )", seqname.replace('\'', "''"));
            col.default_expr = Some(DefaultExpr {
                expr: crate::parser::parse_expression(&expr_text)?,
                expr_text,
            });
            col.identity = def.identity.as_ref().map(|id| {
                if id.always {
                    IdentityKind::Always
                } else {
                    IdentityKind::ByDefault
                }
            });
        } else if col.ty.is_composite() || col.ty.is_array() || col.ty.is_range() {
            if def.default.is_some() {
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
            let (node, _) = resolve(&scope, &c.expr, None, &mut AggCtx::Forbidden, params)?;
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
        let mut keys = Vec::with_capacity(def.keys.len());
        for k in &def.keys {
            match k {
                IndexKey::Column(ord) => keys.push(ResolvedKey::Column(*ord)),
                IndexKey::Expr(e) => {
                    let scope = Scope::single(self, table);
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
                let scope = Scope::single(self, table);
                Some(resolve_boolean_filter(&scope, &p.expr, params)?)
            }
        };
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
        };
        index_entry_keys(columns, colls, rindex, storage_key, row, &env)
    }

    /// A row's uniqueness-probe prefix for one index (spec/design/indexes.md §8), building the
    /// unmetered eval env internally (as [`index_entries`](Self::index_entries)).
    pub(crate) fn index_prefix(
        &self,
        columns: &[Column],
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
        };
        index_prefix_key(columns, colls, rindex, row, &env)
    }

    /// A candidate row's arbiter key for `ON CONFLICT` (spec/design/upsert.md §3), building the
    /// unmetered eval env internally (an expression-index arbiter evaluates its keys — as
    /// [`index_prefix`](Self::index_prefix)).
    pub(crate) fn arbiter_probe_key(
        &self,
        arb: &Arbiter,
        pk: &[(usize, Type)],
        colls: &[Option<std::sync::Arc<Collation>>],
        columns: &[Column],
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
        };
        arbiter_key(arb, pk, colls, columns, rindexes, row, &env)
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
        let mut out = Vec::with_capacity(table.columns.len());
        for col in &table.columns {
            match &col.default_expr {
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
                };
                rx.eval(&[], &env, meter)
            }
            None => Ok(col.default.clone().unwrap_or(Value::Null)),
        }
    }

    /// Run a `DROP TABLE [IF EXISTS] a [, …] [CASCADE | RESTRICT]`: remove each named table's
    /// definition and row store from the catalog (keyed by lower-cased name). Two-phase /
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
        // drops `a` once and succeeds); `seen` is the set of lowercased keys actually being dropped.
        let mut targets: Vec<(String, Scope)> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for name in &dt.names {
            let key = name.to_ascii_lowercase();
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
                if self.find_index(name).is_some() {
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
        // ---- Phase 2: FK dependency check (RESTRICT) / removal collection (CASCADE). Only a
        // persistent table can be an FK parent (a temp table never is, §8), so the scan runs over the
        // persistent snapshot; a dependent whose referencing table is itself in the drop set does not
        // count (the drop-set exclusion is the whole `seen` set, so `DROP TABLE parent, child`
        // succeeds even under RESTRICT).
        let deps = self.read_snap().foreign_key_dependents_excluding(&seen);
        let cascade_removals = if dt.cascade {
            deps
        } else {
            // RESTRICT (the default, and the bare form's behavior): an external FK dependent blocks
            // the drop with 2BP01 — the same message the single-table check produced.
            if let Some(d) = deps.first() {
                return Err(EngineError::new(
                    SqlState::DependentObjectsStillExist,
                    format!(
                        "cannot drop table {} because other objects depend on it: constraint {} on table {}",
                        d.dropped_name, d.fk_name, d.ref_table_name
                    ),
                ));
            }
            Vec::new()
        };
        // ---- Phase 3: apply. CASCADE first drops each surviving table's now-dangling FK constraint
        // (in place, preserving its rows). A FK only ever lives on a persistent table (temp tables
        // reject FKs at CREATE), so the removal routes to the main working snapshot.
        for d in &cascade_removals {
            self.working_mut()
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
    /// lowercased `<table>_<col>..._idx` with the smallest free suffix. The index is then
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
        self.check_table_qualifier(ci.db.as_deref(), &ci.table)?; // attached-databases.md §3
        let attach_name: Option<String> = if is_attachment_scope(ci.db.as_deref()) {
            ci.db.as_ref().map(|d| d.to_ascii_lowercase())
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
        let table_key = table.name.to_ascii_lowercase();
        let columns = table.columns.clone();
        // Refuse building a collated index on a version-skewed table (slice 2d, collation.md §12,
        // XX002): the new B-tree would be pinned inconsistently with the file's other structures.
        self.ensure_collations_writable(&columns)?;
        // Per-column frozen collations for the collated text key form (§2.12); `None` everywhere
        // for a C-only / non-text table (the fast path).
        let colls = self.column_collations(&columns);
        // Resolve the access method (spec/design/gin.md §3): the default / `btree` is the ordered
        // B-tree, `gin` a GIN inverted index; an unknown method is 42704. Resolved here (not in the
        // parser) so the error is the resolve-time undefined_object, after the table-exists check
        // and before the column checks.
        let kind = match ci.using.as_deref().map(str::to_ascii_lowercase).as_deref() {
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
                    // A subquery is not a deterministic function of the row — 0A000 (the resolver
                    // admits an uncorrelated one, so it is rejected here, before resolution).
                    if index_expr_has_subquery(expr) {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "cannot use subquery in index expression".to_string(),
                        ));
                    }
                    // Resolve against the table (an aggregate 42803 / window 42P20 / bind parameter
                    // 42P02 fall out of the resolver, as for a CHECK).
                    let scope = Scope::single(self, table);
                    let mut pt = ParamTypes::default();
                    let (_node, rtype) =
                        resolve(&scope, expr, None, &mut AggCtx::Forbidden, &mut pt)?;
                    // Immutability (§2): a non-immutable seam/sequence/current_setting call, a
                    // session-timezone-dependent expression (one that reads or produces a
                    // `timestamptz` — conservatively fail-closed), or a resolved STABLE node (the
                    // runtime text→date cast, flagged at its birth — `ParamTypes::nonimmutable`),
                    // is 42P17.
                    let refs = check_referenced_columns(expr, &columns);
                    let tz_hazard = matches!(rtype, ResolvedType::Timestamptz)
                        || refs
                            .iter()
                            .any(|&i| columns[i].ty.as_scalar() == Some(ScalarType::Timestamptz));
                    if index_expr_nonimmutable_call(expr) || tz_hazard || pt.nonimmutable {
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
                    ci_keys.push(IndexKey::Expr(IndexKeyExpr {
                        expr_text: text.clone(),
                        expr: expr.clone(),
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
            let ty = &columns[idx].ty;
            match kind {
                IndexKind::Btree => {
                    if !ty.is_integer()
                        && !ty.is_bool()
                        && !ty.is_text()
                        && !ty.is_bytea()
                        && !ty.is_decimal()
                        && !ty.is_uuid()
                        && !ty.is_timestamp()
                        && !ty.is_timestamptz()
                        && !ty.is_date()
                        && !ty.is_interval()
                        && !ty.is_float()
                        && !ty.is_range()
                        && !is_array_keyable(ty)
                    {
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
                    // GIN needs an operator class for the column type: only an array has one (else
                    // 42704, no default opclass), and only a FIXED-WIDTH KEY-ENCODABLE element type
                    // (else 0A000) — the GIN term IS that element's key encoding (gin.md §3/§4), so
                    // the admitted set is the integers, boolean, uuid, date, timestamp, timestamptz
                    // (interval's GIN-element support is a separate follow-on — gin.md §3/§10).
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
                // Structural pre-walk: a subquery is 0A000 and a bind parameter 42P02 (both admitted
                // by the resolver, so caught here). The aggregate 42803 / window 42P20 / non-boolean
                // 42804 rejections then fall out of the Forbidden-context boolean resolve below.
                reject_index_predicate_structure(&pred.expr)?;
                let scope = Scope::single(self, table);
                let mut pt = ParamTypes::default();
                let _node = resolve_boolean_filter(&scope, &pred.expr, &mut pt)?;
                // Immutability (§9), the same rule an expression key carries: a non-immutable
                // seam/clock/sequence call, a session-timezone-dependent subexpression (one that
                // references a `timestamptz` column or produces a `timestamptz` value — conservatively
                // fail-closed), or a resolved STABLE node (the runtime text→date cast,
                // `ParamTypes::nonimmutable`), is 42P17.
                let refs = check_referenced_columns(&pred.expr, &columns);
                let tz_hazard = refs
                    .iter()
                    .any(|&i| columns[i].ty.as_scalar() == Some(ScalarType::Timestamptz));
                if index_expr_nonimmutable_call(&pred.expr) || tz_hazard || pt.nonimmutable {
                    return Err(EngineError::new(
                        SqlState::InvalidObjectDefinition,
                        "functions in index predicate must be marked IMMUTABLE".to_string(),
                    ));
                }
                Some(IndexKeyExpr {
                    expr_text: pred.text.clone(),
                    expr: pred.expr.clone(),
                })
            }
        };
        // `relation_taken` checks the namespace of the target scope: an attachment's OWN snapshot for an
        // attached table (each attached database is an independent namespace, §3), else the temp-aware
        // implicit namespace.
        let relation_taken = |n: &str| -> bool {
            if let Some(name) = &attach_name {
                let as_snap = self
                    .attach_read_snap(name)
                    .expect("attachment resolved above");
                as_snap.table(n).is_some() || as_snap.find_index(n).is_some()
            } else {
                self.relation_exists(n)
            }
        };
        let name = match &ci.name {
            Some(n) => {
                check_reserved_name("index", n)?;
                if relation_taken(n) {
                    return Err(EngineError::new(
                        SqlState::DuplicateTable,
                        format!("relation already exists: {n}"),
                    ));
                }
                n.clone()
            }
            None => {
                // PG's ChooseIndexName / ChooseIndexColumnNames (probed): lowercased table + one
                // name part per key element (list order, duplicates included) + "idx", then the
                // smallest free suffix. A column key's part is the column name; a bare-function-call
                // expression's is the function name (`lower(email)` → `lower`); any other
                // expression's is the literal `expr` (indexes.md §2).
                let mut base = table_key.clone();
                for elem in &ci.keys {
                    base.push('_');
                    base.push_str(&index_name_part(elem));
                }
                base.push_str("_idx");
                let mut candidate = base.clone();
                let mut suffix = 0u32;
                while relation_taken(&candidate) {
                    suffix += 1;
                    candidate = format!("{base}{suffix}");
                }
                candidate
            }
        };

        let def = IndexDef {
            name,
            keys: ci_keys,
            unique: ci.unique,
            kind,
            predicate,
        };
        // The build scan (cost.md §3): page_read per table-tree node + storage_row_read per
        // row. The touched set is the columns the key elements read — an index column for a
        // column key, or every column an expression key references (which may be variable-width,
        // so a spilled value adds its `value_decompress` slabs — indexes.md §5). An empty table
        // charges 0. Entries are computed here against the pre-index store; the writes below are
        // unmetered. An expression key evaluating with an error aborts the build (nothing is
        // registered — indexes.md §4), preserving all-or-nothing.
        let mut meter = self.session.new_meter();
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
        let rindex = self.resolve_index(table, &def)?;
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
            };
            let store = self.store_scoped(ci.db.as_deref(), &ci.table);
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
                    && let Some(prefix) = index_prefix_key(&columns, &colls, &rindex, &row, &env)?
                    && !seen_prefixes.insert(prefix)
                {
                    return Err(EngineError::unique_violation(&ci.table, &def.name));
                }
                entries.extend(index_entry_keys(
                    &columns, &colls, &rindex, &key, &row, &env,
                )?);
            }
        }
        meter.guard()?;

        let name_key = def.name.to_ascii_lowercase();
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
        // Insert sorted by entry key (indexes.md §1): every insert is then a right-edge append,
        // so the built tree packs ~full instead of splintering under the storage-key order the
        // scan produced (random in entry-key space). Part of the byte contract — the sort fixes
        // the built tree's shape across cores.
        entries.sort_unstable();
        for ek in entries {
            assert!(
                istore.insert(ek, Vec::new())?,
                "index entry keys are unique (storage-key suffix)"
            );
        }
        Ok(Outcome::Statement {
            cost: meter.accrued,
            rows_affected: None,
        })
    }

    /// Run a DROP INDEX (spec/design/indexes.md §2): a table's name is 42809, a missing one
    /// 42704. A pure catalog edit — zero cost, like DROP TABLE. The index is resolved along the
    /// resolution walk (session-local → persistent — temp-tables.md §8) and removed from the snapshot
    /// that owns it, so dropping a temp table's index makes zero file writes.
    pub(crate) fn execute_drop_index(&mut self, di: DropIndex) -> Result<Outcome> {
        // `table` covers both scopes, so DROP INDEX naming a table is 42809 regardless of kind.
        if self.table(&di.name).is_some() {
            return Err(EngineError::new(
                SqlState::WrongObjectType,
                format!("{} is not an index", di.name),
            ));
        }
        let name_key = di.name.to_ascii_lowercase();
        // An index that backs an EXCLUDE constraint cannot be dropped directly — the constraint owns
        // it (the UNIQUE-backing precedent; jed has no ALTER TABLE … DROP CONSTRAINT yet, so the
        // index lives until DROP TABLE). 2BP01, matching PG's "cannot drop index … because
        // constraint … requires it" (spec/design/gist.md §7).
        if let Some(table_key) = self.find_index(&di.name).map(|(tk, _)| tk.to_string()) {
            if let Some(t) = self.table(&table_key) {
                if t.exclusions
                    .iter()
                    .any(|e| e.index.eq_ignore_ascii_case(&di.name))
                {
                    return Err(EngineError::new(
                        SqlState::DependentObjectsStillExist,
                        format!(
                            "cannot drop index {} because constraint {} on table {} requires it",
                            di.name, di.name, t.name
                        ),
                    ));
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
            if fields.iter().any(|g| g.name.eq_ignore_ascii_case(&f.name)) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("attribute {} specified more than once", f.name),
                ));
            }
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
                    let (s, d, vlen) = resolve_type_and_typmod(&f.type_name, &f.type_mod)?;
                    (Type::Scalar(s), d, vlen)
                } else if crate::range::range_by_name(&f.type_name).is_some() {
                    // A range-typed composite field (a `range` inside `CREATE TYPE`) is deferred
                    // this slice (only range *columns* are storable — spec/design/ranges.md §3); the
                    // type name IS known, so this is 0A000, not the 42704 below.
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "a range-typed composite field ({}) is not supported yet",
                            f.type_name
                        ),
                    ));
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
        let key = dt.name.to_ascii_lowercase();
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
                let new_def = apply_seq_alter(&existing, &options, restart)?;
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
            let expr_text = format!(
                "nextval ( '{}' )",
                new_name.to_ascii_lowercase().replace('\'', "''")
            );
            let expr = crate::parser::parse_expression(&expr_text)?;
            // Route to the owner's scope so a renamed owned TEMP sequence rewrites its column default
            // in the temp snapshot (temp-tables.md §8).
            self.set_column_default_expr_routed(
                &owner.table.to_ascii_lowercase(),
                owner.column as usize,
                DefaultExpr { expr_text, expr },
            );
        }
        // Capture the owning scope BEFORE the remove: after dropping the old key the new name is in no
        // scope, so a post-remove route would wrongly default to the main image (temp-tables.md §8).
        let is_temp = self.is_temp_sequence(&existing.name);
        let old_key = existing.name.to_ascii_lowercase();
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
        // A timestamptz-dependent EXPRESSION is also non-immutable (its value depends on the
        // session time zone) — conservatively fail-closed (indexes.md §2).
        let e2 = execute(&mut db, "CREATE INDEX ON t ((ts + interval '1 hour'))").unwrap_err();
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
    //! implication + timestamptz hazard; on-disk byte round-trip; catalog introspection). The
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

    // A timestamptz-referencing predicate is conservatively 42P17 (the session-tz hazard, extended
    // from expression keys — a documented jed divergence, indexes.md §9). A non-boolean predicate is
    // 42804; a partial GIN index is 0A000.
    #[test]
    fn predicate_rejections() {
        let mut db = Engine::new();
        execute(
            &mut db,
            "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz, a i32, arr i32[])",
        )
        .unwrap();
        let tz = execute(&mut db, "CREATE INDEX ON t (a) WHERE ts IS NULL").unwrap_err();
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
