//! Set-returning functions in FROM — planning and row production (mirrors impl/go srf.go): resolve_srf
//! and the per-family resolvers (generate_series, unnest, the JSON each/record/populate functions,
//! JSON_TABLE, and the jed_* catalog relations) plus their row producers — as Engine methods.

use super::*;

impl Engine {
    /// Resolve a FROM-clause set-returning function call into a **synthetic one-column relation**
    /// plus its resolved argument expressions and the [`SrfKind`] selecting its generator
    /// (spec/design/functions.md §10, array-functions.md §9). Two SRFs exist: `generate_series`
    /// (2/3 integer args) and the polymorphic `unnest(anyarray)` (1 array arg); any other name →
    /// `42883`. Non-LATERAL: the args resolve against an EMPTY-local-rels scope whose `parent` is
    /// the enclosing query, so `$N` and correlated outer columns resolve while a sibling FROM table
    /// does not (42703/42P01). The produced column's NAME follows PostgreSQL's single-column
    /// function-alias rule: the table alias when one is given (`unnest(xs) AS g` ⇒ column `g`),
    /// else the function name. Returns `(synthetic table, resolved args, kind)`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_srf<'a>(
        &'a self,
        name: &str,
        args: &[Expr],
        alias: Option<&str>,
        column_defs: Option<&[TypeFieldDef]>,
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        // The args see only params/outer — never sibling FROM tables (non-LATERAL); CTE bindings
        // are inherited so an arg subquery can reference a CTE (cte.md §2).
        let arg_scope = Scope {
            rels: Vec::new(),
            parent,
            catalog: self,
            allow_subquery: true,
            ctes,
            merges: Vec::new(),
            hidden: Vec::new(),
        };
        // Record-returning functions (R1, json-table.md §2): json[b]_to_record → one record row,
        // json[b]_to_recordset → setof record. They take their column shape from the C0 col-def list
        // `AS t(col type, …)`. Dispatched first, before the col-def-list guard below.
        let lc = name.to_ascii_lowercase();
        if matches!(
            lc.as_str(),
            "json_to_record" | "jsonb_to_record" | "json_to_recordset" | "jsonb_to_recordset"
        ) {
            let jsonb = lc.starts_with("jsonb");
            let set = lc.ends_with("set");
            return self.resolve_json_record(
                &lc,
                jsonb,
                set,
                args,
                alias,
                column_defs,
                &arg_scope,
                ptypes,
            );
        }
        // A column-definition list is valid ONLY on a record-returning function (PG).
        if column_defs.is_some() {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                format!(
                    "a column definition list is only allowed for a record-returning function, not {name}"
                ),
            ));
        }
        if name.eq_ignore_ascii_case("generate_series") {
            return self.resolve_generate_series(args, alias, &arg_scope, ptypes);
        }
        if name.eq_ignore_ascii_case("unnest") {
            return self.resolve_unnest(args, alias, &arg_scope, ptypes);
        }
        // json/jsonb two-column SRFs (B3, json-sql-functions.md §3): jsonb_each → (key text, value
        // jsonb), jsonb_each_text → (key text, value text). The json variants (verbatim sub-text,
        // json.md §4) are a deferred 0A000 follow-on. Built on the C0 multi-column synthetic table.
        let lname = name.to_ascii_lowercase();
        match lname.as_str() {
            "jsonb_each" => {
                return self.resolve_json_each(
                    &lname,
                    SrfKind::JsonbEach,
                    Type::Scalar(ScalarType::Jsonb),
                    args,
                    alias,
                    &arg_scope,
                    ptypes,
                );
            }
            "jsonb_each_text" => {
                return self.resolve_json_each(
                    &lname,
                    SrfKind::JsonbEachText,
                    Type::Scalar(ScalarType::Text),
                    args,
                    alias,
                    &arg_scope,
                    ptypes,
                );
            }
            "json_each" | "json_each_text" => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!("{lname} is not supported yet; use the jsonb variant"),
                ));
            }
            "jsonb_path_query" => {
                let (ctx, path) = resolve_jsonpath_args(
                    &arg_scope,
                    &lname,
                    args,
                    &mut AggCtx::Forbidden,
                    ptypes,
                )?;
                let table = srf_table(&lname, alias, Type::Scalar(ScalarType::Jsonb));
                return Ok((table, vec![ctx, path], SrfKind::JsonbPathQuery));
            }
            // json[b]_populate_record(set) (R2, json-table.md §2): like json[b]_to_record(set) but the
            // column shape comes from the COMPOSITE TYPE of the (typically NULL) first argument.
            "json_populate_record"
            | "jsonb_populate_record"
            | "json_populate_recordset"
            | "jsonb_populate_recordset" => {
                let jsonb = lname.starts_with("jsonb");
                let set = lname.ends_with("set");
                return self
                    .resolve_json_populate(&lname, jsonb, set, args, alias, &arg_scope, ptypes);
            }
            _ => {}
        }
        // json/jsonb single-column SRFs (B2, json-sql-functions.md §3). The json `array_elements`
        // variants preserve the verbatim sub-text (json.md §4) and are a deferred 0A000 follow-on,
        // like the json accessor operators; the jsonb variants + `json_object_keys` ship here.
        if let Some((kind, col_ty, jsonb)) = match lname.as_str() {
            "jsonb_array_elements" => Some((
                SrfKind::JsonbArrayElements,
                Type::Scalar(ScalarType::Jsonb),
                true,
            )),
            "jsonb_array_elements_text" => Some((
                SrfKind::JsonbArrayElementsText,
                Type::Scalar(ScalarType::Text),
                true,
            )),
            "jsonb_object_keys" => Some((
                SrfKind::JsonbObjectKeys,
                Type::Scalar(ScalarType::Text),
                true,
            )),
            "json_object_keys" => Some((
                SrfKind::JsonObjectKeys,
                Type::Scalar(ScalarType::Text),
                false,
            )),
            "json_array_elements" | "json_array_elements_text" => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!("{lname} is not supported yet; use the jsonb variant"),
                ));
            }
            _ => None,
        } {
            return self
                .resolve_json_srf(&lname, kind, col_ty, jsonb, args, alias, &arg_scope, ptypes);
        }
        Err(EngineError::new(
            SqlState::UndefinedFunction,
            format!("function does not exist: {name}"),
        ))
    }

    /// Resolve a json/jsonb single-column SRF (B2, json-sql-functions.md §3): the one argument is a
    /// json/jsonb value (a bare string literal adapts to the expected document type). The synthetic
    /// column's type is fixed (`jsonb`/`text`). A NULL argument yields zero rows at exec.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_json_srf(
        &self,
        name: &str,
        kind: SrfKind,
        col_ty: Type,
        jsonb: bool,
        args: &[Expr],
        alias: Option<&str>,
        arg_scope: &Scope,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        if args.len() != 1 {
            return Err(no_func_overload(name));
        }
        let want = if jsonb {
            ScalarType::Jsonb
        } else {
            ScalarType::Json
        };
        let (rarg, t) = resolve(
            arg_scope,
            &args[0],
            Some(want),
            &mut AggCtx::Forbidden,
            ptypes,
        )?;
        let ok = match (&t, jsonb) {
            (ResolvedType::Jsonb, true) | (ResolvedType::Json, false) | (ResolvedType::Null, _) => {
                true
            }
            _ => false,
        };
        if !ok {
            return Err(no_func_overload(name));
        }
        let table = srf_table(name, alias, col_ty);
        Ok((table, vec![rarg], kind))
    }

    /// Resolve a json/jsonb TWO-column SRF (B3 — `jsonb_each` / `jsonb_each_text`,
    /// json-sql-functions.md §3): the one argument is a jsonb value (a bare string literal adapts).
    /// The synthetic relation has the fixed columns `key text` and `value <value_ty>` (the C0
    /// multi-column synthetic table). A non-object argument → `22023` at exec; a NULL → zero rows.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_json_each(
        &self,
        name: &str,
        kind: SrfKind,
        value_ty: Type,
        args: &[Expr],
        alias: Option<&str>,
        arg_scope: &Scope,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        if args.len() != 1 {
            return Err(no_func_overload(name));
        }
        let (rarg, t) = resolve(
            arg_scope,
            &args[0],
            Some(ScalarType::Jsonb),
            &mut AggCtx::Forbidden,
            ptypes,
        )?;
        if !matches!(t, ResolvedType::Jsonb | ResolvedType::Null) {
            return Err(no_func_overload(name));
        }
        let table = srf_table_cols(
            name,
            alias,
            vec![("key", Type::Scalar(ScalarType::Text)), ("value", value_ty)],
        );
        Ok((table, vec![rarg], kind))
    }

    /// Resolve a json/jsonb RECORD-returning SRF (R1 — `json[b]_to_record` / `json[b]_to_recordset`,
    /// json-table.md §2): the one argument is a json/jsonb document; the output columns come from the
    /// C0 col-def list `AS t(col type, …)` (required — else 42601). The synthetic table's columns are
    /// the declared types (a composite/array column type is a deferred `0A000`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_json_record(
        &self,
        name: &str,
        jsonb: bool,
        set: bool,
        args: &[Expr],
        alias: Option<&str>,
        column_defs: Option<&[TypeFieldDef]>,
        arg_scope: &Scope,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        if args.len() != 1 {
            return Err(no_func_overload(name));
        }
        let want = if jsonb {
            ScalarType::Jsonb
        } else {
            ScalarType::Json
        };
        let (rarg, t) = resolve(
            arg_scope,
            &args[0],
            Some(want),
            &mut AggCtx::Forbidden,
            ptypes,
        )?;
        let ok = matches!(
            (&t, jsonb),
            (ResolvedType::Jsonb, true) | (ResolvedType::Json, false) | (ResolvedType::Null, _)
        );
        if !ok {
            return Err(no_func_overload(name));
        }
        let defs = column_defs.ok_or_else(|| {
            EngineError::new(
                SqlState::SyntaxError,
                format!("a column definition list is required for function {name}"),
            )
        })?;
        let mut columns = Vec::with_capacity(defs.len());
        for d in defs {
            // A composite/array column type in the col-def list is a deferred 0A000 follow-on.
            if d.type_name.ends_with("[]") || self.composite_type(&d.type_name).is_some() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a composite/array column in a record column-definition list is not supported yet",
                ));
            }
            let (st, decimal, varchar_len) = resolve_type_and_typmod(&d.type_name, &d.type_mod)?;
            columns.push(Column {
                name: d.name.clone(),
                ty: Type::Scalar(st),
                decimal,
                varchar_len,
                primary_key: false,
                not_null: false,
                default: None,
                default_expr: None,
                identity: None,
                collation: None,
            });
        }
        let table = Box::new(Table {
            name: alias.unwrap_or(name).to_string(),
            columns,
            pk: Vec::new(),
            checks: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            exclusions: Vec::new(),
        });
        Ok((table, vec![rarg], SrfKind::JsonRecord { jsonb, set }))
    }

    /// Resolve a json/jsonb POPULATE-RECORD SRF (R2 — `json[b]_populate_record(set)`, json-table.md
    /// §2): the FIRST argument is a (typically NULL) value whose COMPOSITE TYPE supplies the output
    /// column shape; the SECOND is the json/jsonb document. Reuses the R1 row machinery
    /// (`SrfKind::JsonRecord`) — only the column source differs (a composite type vs a col-def list).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_json_populate(
        &self,
        name: &str,
        jsonb: bool,
        set: bool,
        args: &[Expr],
        alias: Option<&str>,
        arg_scope: &Scope,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        if args.len() != 2 {
            return Err(no_func_overload(name));
        }
        // The base argument's COMPOSITE type fixes the columns (its value is unused — usually NULL).
        let (_base, bt) = resolve(arg_scope, &args[0], None, &mut AggCtx::Forbidden, ptypes)?;
        let ctype = match &bt {
            ResolvedType::Composite(r) => {
                // A named composite supplies the columns; an anonymous `record` base is `0A000`.
                let tname = r.name.as_deref().ok_or_else(|| {
                    EngineError::new(
                        SqlState::FeatureNotSupported,
                        "an anonymous record base is not supported yet",
                    )
                })?;
                self.composite_type(tname).ok_or_else(|| {
                    EngineError::new(SqlState::UndefinedObject, "composite type no longer exists")
                })?
            }
            _ => {
                return Err(EngineError::new(
                    SqlState::DatatypeMismatch,
                    format!("the first argument of {name} must be a composite type"),
                ));
            }
        };
        let columns: Vec<Column> = ctype
            .fields
            .iter()
            .map(|f| Column {
                name: f.name.clone(),
                ty: f.ty.clone(),
                decimal: f.decimal,
                varchar_len: f.varchar_len,
                primary_key: false,
                not_null: false,
                default: None,
                default_expr: None,
                identity: None,
                collation: None,
            })
            .collect();
        let want = if jsonb {
            ScalarType::Jsonb
        } else {
            ScalarType::Json
        };
        let (doc, dt) = resolve(
            arg_scope,
            &args[1],
            Some(want),
            &mut AggCtx::Forbidden,
            ptypes,
        )?;
        if !matches!(
            (&dt, jsonb),
            (ResolvedType::Jsonb, true) | (ResolvedType::Json, false) | (ResolvedType::Null, _)
        ) {
            return Err(no_func_overload(name));
        }
        let table = Box::new(Table {
            name: alias.unwrap_or(name).to_string(),
            columns,
            pk: Vec::new(),
            checks: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            exclusions: Vec::new(),
        });
        // The SRF arg is the json DOCUMENT (the base value is unused); reuse the R1 row generator.
        Ok((table, vec![doc], SrfKind::JsonRecord { jsonb, set }))
    }

    /// Resolve a `JSON_TABLE(ctx, path COLUMNS (…))` source (T1, json-table.md §3) → its synthetic
    /// relation (the flattened columns), the `[ctx]` arg, and the resolved [`JtPlan`].
    pub(crate) fn resolve_json_table<'a>(
        &'a self,
        jt: &JsonTable,
        alias: Option<&str>,
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, JtPlan)> {
        // The ctx / root path see only params + the lateral prefix (never sibling columns of THIS
        // relation) — an empty-local-rels scope chained to `parent`, exactly like an SRF (§44).
        let arg_scope = Scope {
            rels: Vec::new(),
            parent,
            catalog: self,
            allow_subquery: true,
            ctes,
            merges: Vec::new(),
            hidden: Vec::new(),
        };
        // The context item (json / jsonb / text, coerced to a jsonb document at eval).
        let (rctx, ctx_ty) = resolve(
            &arg_scope,
            &jt.ctx,
            Some(ScalarType::Jsonb),
            &mut AggCtx::Forbidden,
            ptypes,
        )?;
        if !matches!(
            ctx_ty,
            ResolvedType::Jsonb | ResolvedType::Json | ResolvedType::Text | ResolvedType::Null
        ) {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                format!(
                    "the context item of JSON_TABLE must be json/jsonb/text, not {}",
                    ctx_ty.type_name()
                ),
            ));
        }
        // The root path — a constant jsonpath (a string literal compiles).
        let (rpath, pt) = resolve(
            &arg_scope,
            &jt.path,
            Some(ScalarType::JsonPath),
            &mut AggCtx::Forbidden,
            ptypes,
        )?;
        if !matches!(pt, ResolvedType::JsonPath) {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                "the path of JSON_TABLE must be a constant jsonpath",
            ));
        }
        let root_path = match &rpath {
            RExpr::ConstJsonPath(s) => s.clone(),
            _ => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a non-constant JSON_TABLE path is not supported",
                ));
            }
        };
        let mut out_columns: Vec<Column> = Vec::new();
        let columns = self.resolve_jt_columns(&jt.columns, &mut out_columns)?;
        let width = out_columns.len();
        let table = Box::new(Table {
            name: alias.unwrap_or("json_table").to_string(),
            columns: out_columns,
            pk: Vec::new(),
            checks: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            exclusions: Vec::new(),
        });
        Ok((
            table,
            vec![rctx],
            JtPlan {
                root_path,
                width,
                columns,
            },
        ))
    }

    /// Recursively resolve a `JSON_TABLE` `COLUMNS` tree, flattening the leaf columns into
    /// `out_columns` (pre-order, declaration order) and assigning each its flat output index.
    pub(crate) fn resolve_jt_columns(
        &self,
        cols: &[JtColumn],
        out_columns: &mut Vec<Column>,
    ) -> Result<Vec<JtCol>> {
        let mut resolved = Vec::with_capacity(cols.len());
        for col in cols {
            match col {
                JtColumn::Ordinality { name } => {
                    let idx = out_columns.len();
                    out_columns.push(jt_column(name, ScalarType::Int32, None));
                    resolved.push(JtCol::Ordinality { idx });
                }
                JtColumn::Regular {
                    name,
                    type_name,
                    array,
                    path,
                    wrapper,
                    keep_quotes,
                    on_empty,
                    on_error,
                } => {
                    if *array {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "an array JSON_TABLE column is not supported yet",
                        ));
                    }
                    let st = jt_scalar_type(self, type_name)?;
                    if !*keep_quotes {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "JSON_TABLE OMIT QUOTES is not supported yet",
                        ));
                    }
                    let query = matches!(st, ScalarType::Json | ScalarType::Jsonb);
                    if !query && !matches!(wrapper, JsonWrapper::Without) {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "a WRAPPER on a scalar JSON_TABLE column is not supported yet",
                        ));
                    }
                    let compiled = jt_compile_path(path.as_deref(), name)?;
                    let idx = out_columns.len();
                    out_columns.push(jt_column(name, st, None));
                    resolved.push(JtCol::Regular {
                        idx,
                        returning: st,
                        decimal: None,
                        path: compiled,
                        query,
                        wrapper: *wrapper,
                        on_empty: on_empty.unwrap_or(JsonOnBehavior::Null),
                        on_error: on_error.unwrap_or(JsonOnBehavior::Null),
                    });
                }
                JtColumn::Exists {
                    name,
                    type_name,
                    path,
                    on_error,
                } => {
                    let st = jt_scalar_type(self, type_name)?;
                    let compiled = jt_compile_path(path.as_deref(), name)?;
                    let idx = out_columns.len();
                    out_columns.push(jt_column(name, st, None));
                    resolved.push(JtCol::Exists {
                        idx,
                        returning: st,
                        path: compiled,
                        on_error: on_error.unwrap_or(JsonOnBehavior::False),
                    });
                }
                JtColumn::Nested { path, columns } => {
                    let compiled = crate::jsonpath::JsonPath::compile(path)?.render();
                    let nested = self.resolve_jt_columns(columns, out_columns)?;
                    resolved.push(JtCol::Nested {
                        path: compiled,
                        columns: nested,
                    });
                }
            }
        }
        Ok(resolved)
    }

    /// Generate the rows of a `JSON_TABLE` SRF (T1, json-table.md §3) — the default-plan recursive
    /// expansion (parent→child LEFT OUTER, sibling NESTED paths UNIONed).
    pub(crate) fn json_table_rows(
        &self,
        srf: &SrfPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let plan = srf
            .json_table
            .as_ref()
            .expect("a JsonTable SRF carries its plan");
        let ctx = srf.args[0].eval(&[], env, meter)?;
        if matches!(ctx, Value::Null) {
            return Ok(Vec::new());
        }
        let node = json_arg_node(&ctx)?;
        // The root path → the sequence of row items (a structural error here yields no rows).
        let root = crate::jsonpath::JsonPath::compile(&plan.root_path)?;
        let items = crate::jsonpath::eval(&root, &node).unwrap_or_default();
        // Expand the column tree over the root sequence → sparse rows, then materialize.
        let sparse = expand_jt_level(&plan.columns, &items, env, meter)?;
        let mut out = Vec::with_capacity(sparse.len());
        for assignment in sparse {
            meter.guard()?;
            meter.charge(COSTS.generated_row);
            let mut row = vec![Value::Null; plan.width];
            for (idx, v) in assignment {
                row[idx] = v;
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Resolve `generate_series(start, stop[, step])` (spec/design/functions.md §10): 2 or 3
    /// integer args (a wrong arity/type → `42883`). The produced column is typed at the PROMOTED
    /// integer type of the args (PG); a NULL-typed arg contributes no width (the call yields zero
    /// rows at exec). All-NULL defaults i64.
    pub(crate) fn resolve_generate_series(
        &self,
        args: &[Expr],
        alias: Option<&str>,
        arg_scope: &Scope,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        if args.len() != 2 && args.len() != 3 {
            return Err(no_func_overload("generate_series"));
        }
        let mut rargs = Vec::with_capacity(args.len());
        let mut result: Option<ScalarType> = None;
        for a in args {
            let (r, t) = resolve(
                arg_scope,
                a,
                Some(ScalarType::Int64),
                &mut AggCtx::Forbidden,
                ptypes,
            )?;
            match t {
                ResolvedType::Int(st) => {
                    result = Some(match result {
                        Some(prev) if prev.rank() >= st.rank() => prev,
                        _ => st,
                    });
                }
                ResolvedType::Null => {}
                _ => return Err(no_func_overload("generate_series")),
            }
            rargs.push(r);
        }
        let result = result.unwrap_or(ScalarType::Int64);
        let table = srf_table("generate_series", alias, Type::Scalar(result));
        Ok((table, rargs, SrfKind::GenerateSeries))
    }

    /// Resolve `unnest(anyarray)` (spec/design/array-functions.md §9, §13): the single argument must
    /// be an array (binding `ELEM` := its element type, the produced column's type), else `42883`
    /// (a non-array, e.g. `unnest(5)`). A bare untyped `NULL` argument leaves `ELEM` undeterminable
    /// → `42P18` (jed's polymorphic posture, exactly like `array_append(NULL, NULL)`); a *typed*
    /// NULL array (`NULL::i32[]`) resolves and yields zero rows at exec. `ELEM` may be a **scalar
    /// or a composite** (AF7 — `unnest(composite[])`): the synthetic column is typed at the bound
    /// element type directly (`type_from_resolved`), so a composite array produces composite rows
    /// (an anonymous-composite element has no catalog name → `0A000`, not reachable from a typed array).
    pub(crate) fn resolve_unnest(
        &self,
        args: &[Expr],
        alias: Option<&str>,
        arg_scope: &Scope,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, Vec<RExpr>, SrfKind)> {
        if args.len() != 1 {
            return Err(no_func_overload("unnest"));
        }
        let (rarg, t) = resolve(arg_scope, &args[0], None, &mut AggCtx::Forbidden, ptypes)?;
        let elem_ty = match t {
            ResolvedType::Array(elem) => type_from_resolved(&elem)?,
            ResolvedType::Null => return Err(indeterminate_poly()),
            _ => return Err(no_func_overload("unnest")),
        };
        let table = srf_table("unnest", alias, elem_ty);
        Ok((table, vec![rarg], SrfKind::Unnest))
    }

    /// Generate the rows of a `generate_series(start, stop[, step])` FROM-clause source
    /// (spec/design/functions.md §10), as a `Vec` of one-column rows. The args evaluate ONCE
    /// against the outer environment with an empty local row (non-LATERAL — they reference only
    /// params/outer). PostgreSQL semantics: any NULL arg → zero rows; a step of zero → `22023`;
    /// `start > stop` with a positive step (or the reverse) → zero rows; an i64 overflow while
    /// stepping STOPS the series cleanly (no trap). Each generated element charges one
    /// `generated_row` AT THE SOURCE, guarded so a `max_cost` ceiling aborts a runaway series
    /// (54P01) mid-generation before the whole thing materializes (CLAUDE.md §13).
    /// Generate the rows of the `jed_tables` catalog relation (introspection.md §5): one row per
    /// USER table of the scope's pinned catalog snapshot — the canonical (CREATE TABLE-spelled)
    /// name — in ascending lowercased-name order (deterministic, no map-iteration leak; the
    /// multiset is the contract, order without ORDER BY stays unspecified — CLAUDE.md §8). Derived
    /// entirely from the resident catalog: zero `page_read` / `storage_row_read`; each produced row
    /// charges one `generated_row` AT THE SOURCE, guarded so a max_cost ceiling aborts
    /// deterministically (§13).
    pub(crate) fn jed_tables_rows(&self, srf: &SrfPlan, meter: &mut Meter) -> Result<Vec<Row>> {
        let Some(snap) = self.snap_for_scope(&srf.introspect_scope) else {
            // The attachment was valid at plan time but is gone at exec (a detached-then-reused plan).
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("database \"{}\" is not attached", srf.introspect_scope),
            ));
        };
        let mut out: Vec<Row> = Vec::new();
        for t in snap.tables_sorted() {
            meter.guard()?;
            meter.charge(COSTS.generated_row);
            out.push(vec![Value::Text(t.name.clone())]);
        }
        Ok(out)
    }

    /// Generate the rows of the `jed_columns` catalog relation (introspection.md §5): one row per
    /// column of every user table of the scope's snapshot, in (lowercased table name, ordinal)
    /// order. `ordinal` is 1-based CREATE TABLE order; `type` is the canonical type text
    /// (`catalog_type_text`); `not_null` covers a declared NOT NULL and PRIMARY KEY membership;
    /// `pk_ordinal` is the 1-based position in the PRIMARY KEY in KEY order (which may differ from
    /// declaration order — constraints.md §3), NULL for a non-member. Cost mirrors jed_tables_rows.
    pub(crate) fn jed_columns_rows(&self, srf: &SrfPlan, meter: &mut Meter) -> Result<Vec<Row>> {
        let Some(snap) = self.snap_for_scope(&srf.introspect_scope) else {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("database \"{}\" is not attached", srf.introspect_scope),
            ));
        };
        let mut out: Vec<Row> = Vec::new();
        for t in snap.tables_sorted() {
            for (i, c) in t.columns.iter().enumerate() {
                meter.guard()?;
                meter.charge(COSTS.generated_row);
                let pk_ordinal = match t.pk.iter().position(|&ord| ord == i) {
                    Some(k) => Value::Int(k as i64 + 1),
                    None => Value::Null,
                };
                out.push(vec![
                    Value::Text(t.name.clone()),
                    Value::Text(c.name.clone()),
                    Value::Int(i as i64 + 1),
                    Value::Text(catalog_type_text(&c.ty, c.decimal.as_ref(), c.varchar_len)),
                    Value::Bool(c.not_null || c.primary_key),
                    pk_ordinal,
                ]);
            }
        }
        Ok(out)
    }

    /// Generate the rows of the `jed_indexes` catalog relation (introspection.md §5.1): one row per
    /// secondary index of every user table of the scope's snapshot, in (lowercased table name, then
    /// the catalog's ascending index-name order) order. `columns` is the `text[]` of indexed column
    /// names in index-key order (duplicates included); `is_unique` the catalog flag; `method` the
    /// access-method name (`btree` / `gin` / `gist`). Cost mirrors jed_tables_rows (one
    /// `generated_row` per row at the source, guarded; zero page_read / storage_row_read).
    pub(crate) fn jed_indexes_rows(&self, srf: &SrfPlan, meter: &mut Meter) -> Result<Vec<Row>> {
        let Some(snap) = self.snap_for_scope(&srf.introspect_scope) else {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("database \"{}\" is not attached", srf.introspect_scope),
            ));
        };
        let mut out: Vec<Row> = Vec::new();
        for t in snap.tables_sorted() {
            for idx in &t.indexes {
                meter.guard()?;
                meter.charge(COSTS.generated_row);
                // A column key shows its column name; an expression key its canonical text
                // (introspection.md §5.1) — the same `columns text[]` cell.
                let cols: Vec<Value> = idx
                    .keys
                    .iter()
                    .map(|k| match k {
                        IndexKey::Column(ord) => Value::Text(t.columns[*ord].name.clone()),
                        IndexKey::Expr(e) => Value::Text(e.expr_text.clone()),
                    })
                    .collect();
                out.push(vec![
                    Value::Text(idx.name.clone()),
                    Value::Text(t.name.clone()),
                    Value::Array(ArrayVal::one_dim(cols)),
                    Value::Bool(idx.unique),
                    Value::Text(index_method_name(idx.kind).to_string()),
                    // A partial index's predicate canonical text; NULL for a non-partial index
                    // (indexes.md §9).
                    match &idx.predicate {
                        Some(p) => Value::Text(p.expr_text.clone()),
                        None => Value::Null,
                    },
                ]);
            }
        }
        Ok(out)
    }

    pub(crate) fn jed_statistics_rows(&self, srf: &SrfPlan, meter: &mut Meter) -> Result<Vec<Row>> {
        let Some(snap) = self.snap_for_scope(&srf.introspect_scope) else {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("database \"{}\" is not attached", srf.introspect_scope),
            ));
        };
        let mut out = Vec::new();
        for (table_key, column, statistics) in snap.statistics_sorted() {
            let Some(table) = snap.table(table_key) else {
                continue;
            };
            let Some(declared) = table.columns.get(column) else {
                continue;
            };
            meter.guard()?;
            meter.charge(COSTS.generated_row);
            let nonnull = statistics.analyzed_rows - statistics.null_count;
            let average_width = if nonnull == 0 {
                Value::Null
            } else {
                let quotient = statistics.width_sum / nonnull;
                let remainder = statistics.width_sum % nonnull;
                Value::Int(quotient + i64::from(remainder != 0))
            };
            out.push(vec![
                Value::Text(table.name.clone()),
                Value::Text(declared.name.clone()),
                Value::Int(statistics.analyzed_rows),
                Value::Bool(statistics.stale),
                Value::Int(statistics.null_count),
                statistics.distinct_count.map_or(Value::Null, Value::Int),
                Value::Int(statistics.sample_rows as i64),
                average_width,
                Value::Int(statistics.mcv.len() as i64),
                Value::Int(statistics.histogram.len() as i64),
            ]);
        }
        Ok(out)
    }

    /// Generate the rows of the `jed_constraints` catalog relation (introspection.md §5.1): one row
    /// per CHECK / UNIQUE / FK / EXCLUDE constraint of every user table of the scope's snapshot, in
    /// (lowercased table name, then a fixed KIND order — check, unique, foreign_key, exclude — each
    /// already held in ascending lowercased-name order). PRIMARY KEY / NOT NULL are deliberately
    /// absent (they own no named object and are described by jed_columns). A `UNIQUE` constraint IS
    /// its backing unique b-tree index (constraints.md §5), so `type = 'unique'` lists every unique
    /// index; `expression` is the persisted canonical CHECK text (constraints.md §4.5). Cost mirrors
    /// jed_tables_rows.
    pub(crate) fn jed_constraints_rows(
        &self,
        srf: &SrfPlan,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let Some(snap) = self.snap_for_scope(&srf.introspect_scope) else {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("database \"{}\" is not attached", srf.introspect_scope),
            ));
        };
        let text_arr = |names: Vec<String>| {
            Value::Array(ArrayVal::one_dim(
                names.into_iter().map(Value::Text).collect(),
            ))
        };
        let mut out: Vec<Row> = Vec::new();
        for t in snap.tables_sorted() {
            // CHECK: name / table / 'check' / NULL columns / expression text / NULL ref_*.
            for ck in &t.checks {
                meter.guard()?;
                meter.charge(COSTS.generated_row);
                out.push(vec![
                    Value::Text(ck.name.clone()),
                    Value::Text(t.name.clone()),
                    Value::Text("check".to_string()),
                    Value::Null,
                    Value::Text(ck.expr_text.clone()),
                    Value::Null,
                    Value::Null,
                ]);
            }
            // UNIQUE: every unique b-tree index (a UNIQUE constraint IS its unique index).
            for idx in t.indexes.iter().filter(|i| i.unique) {
                meter.guard()?;
                meter.charge(COSTS.generated_row);
                let cols: Vec<String> = idx
                    .keys
                    .iter()
                    .map(|k| match k {
                        IndexKey::Column(ord) => t.columns[*ord].name.clone(),
                        IndexKey::Expr(e) => e.expr_text.clone(),
                    })
                    .collect();
                out.push(vec![
                    Value::Text(idx.name.clone()),
                    Value::Text(t.name.clone()),
                    Value::Text("unique".to_string()),
                    text_arr(cols),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ]);
            }
            // FOREIGN KEY: local columns / referenced (parent) table + columns (rendered from the
            // parent's canonical names — the parent always exists, it cannot be dropped while
            // referenced, constraints.md §6.10).
            for fk in &t.foreign_keys {
                meter.guard()?;
                meter.charge(COSTS.generated_row);
                let local: Vec<String> = fk
                    .columns
                    .iter()
                    .map(|&ord| t.columns[ord].name.clone())
                    .collect();
                let parent = snap.table(&fk.ref_table);
                let ref_table = parent
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| fk.ref_table.clone());
                let ref_cols: Vec<String> = fk
                    .ref_columns
                    .iter()
                    .map(|&ord| {
                        parent
                            .and_then(|p| p.columns.get(ord))
                            .map(|c| c.name.clone())
                            .unwrap_or_default()
                    })
                    .collect();
                out.push(vec![
                    Value::Text(fk.name.clone()),
                    Value::Text(t.name.clone()),
                    Value::Text("foreign_key".to_string()),
                    text_arr(local),
                    Value::Null,
                    Value::Text(ref_table),
                    text_arr(ref_cols),
                ]);
            }
            // EXCLUDE: the excluded columns in element order (the &&/= operators are a deferred
            // column addition — introspection.md §5.1).
            for exc in &t.exclusions {
                meter.guard()?;
                meter.charge(COSTS.generated_row);
                let cols: Vec<String> = exc
                    .elements
                    .iter()
                    .map(|el| t.columns[el.column].name.clone())
                    .collect();
                out.push(vec![
                    Value::Text(exc.name.clone()),
                    Value::Text(t.name.clone()),
                    Value::Text("exclude".to_string()),
                    text_arr(cols),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ]);
            }
        }
        Ok(out)
    }

    pub(crate) fn generate_series_rows(
        &self,
        srf: &SrfPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let eval_int = |e: &RExpr, m: &mut Meter| -> Result<Option<i64>> {
            match e.eval(&[], env, m)? {
                Value::Int(n) => Ok(Some(n)),
                Value::Null => Ok(None),
                _ => unreachable!("the resolver restricts generate_series args to integers"),
            }
        };
        let start = eval_int(&srf.args[0], meter)?;
        let stop = eval_int(&srf.args[1], meter)?;
        let step = match srf.args.get(2) {
            None => Some(1),
            Some(e) => eval_int(e, meter)?,
        };
        // Any NULL argument yields zero rows (PG).
        let (start, stop, step) = match (start, stop, step) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Ok(Vec::new()),
        };
        if step == 0 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                "step size cannot be equal to zero",
            ));
        }
        let mut out: Vec<Row> = Vec::new();
        let mut cur = start;
        loop {
            let in_range = if step > 0 { cur <= stop } else { cur >= stop };
            if !in_range {
                break;
            }
            meter.guard()?;
            meter.charge(COSTS.generated_row);
            out.push(vec![Value::Int(cur)]);
            // i64 overflow while stepping ends the series cleanly, matching PostgreSQL.
            match cur.checked_add(step) {
                Some(next) => cur = next,
                None => break,
            }
        }
        Ok(out)
    }

    /// Generate the rows of an `unnest(anyarray)` FROM-clause source (spec/design/array-functions.md
    /// §9), as a `Vec` of one-column rows. The single array argument evaluates ONCE against the
    /// outer environment with an empty local row (non-LATERAL). PostgreSQL semantics: a **NULL
    /// array** yields zero rows; the **empty array** `{}` yields zero rows; otherwise one row per
    /// element in **flattened row-major order** (a multidimensional array flattens; a NULL element
    /// is produced as a NULL row). Each produced element charges one `generated_row` AT THE SOURCE,
    /// guarded so a `max_cost` ceiling aborts a runaway unnest (54P01) mid-generation, exactly like
    /// `generate_series` (CLAUDE.md §13).
    /// Generate the rows of a json/jsonb single-column SRF (B2, json-sql-functions.md §3). A NULL
    /// argument yields zero rows (`empty_on_null`). `array_elements[_text]` over a non-array, or
    /// `object_keys` over a non-object, is `22023`. Each produced row charges one `generated_row`.
    pub(crate) fn json_srf_rows(
        &self,
        srf: &SrfPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let arg = srf.args[0].eval(&[], env, meter)?;
        if matches!(arg, Value::Null) {
            return Ok(Vec::new());
        }
        let node = json_arg_node(&arg)?;
        let mut out: Vec<Row> = Vec::new();
        match srf.kind {
            SrfKind::JsonbArrayElements | SrfKind::JsonbArrayElementsText => {
                let elems = match &node {
                    JsonNode::Array(e) => e,
                    _ => {
                        return Err(EngineError::new(
                            SqlState::InvalidParameterValue,
                            "cannot extract elements from a scalar",
                        ));
                    }
                };
                for e in elems {
                    meter.guard()?;
                    meter.charge(COSTS.generated_row);
                    let v = if matches!(srf.kind, SrfKind::JsonbArrayElementsText) {
                        json::node_to_text(e).map_or(Value::Null, Value::Text)
                    } else {
                        Value::Jsonb(e.clone())
                    };
                    out.push(vec![v]);
                }
            }
            SrfKind::JsonbObjectKeys | SrfKind::JsonObjectKeys => {
                let members = match &node {
                    JsonNode::Object(m) => m,
                    _ => {
                        return Err(EngineError::new(
                            SqlState::InvalidParameterValue,
                            "cannot call jsonb_object_keys on a non-object",
                        ));
                    }
                };
                for (k, _) in members {
                    meter.guard()?;
                    meter.charge(COSTS.generated_row);
                    out.push(vec![Value::Text(k.clone())]);
                }
            }
            SrfKind::JsonbEach | SrfKind::JsonbEachText => {
                let members = match &node {
                    JsonNode::Object(m) => m,
                    _ => {
                        return Err(EngineError::new(
                            SqlState::InvalidParameterValue,
                            format!("cannot call {} on a non-object", srf_kind_name(srf.kind)),
                        ));
                    }
                };
                for (k, v) in members {
                    meter.guard()?;
                    meter.charge(COSTS.generated_row);
                    // (key text, value): jsonb_each keeps the value node; _text renders ->>-style
                    // (a string member's raw content, a JSON null → SQL NULL, else canonical).
                    let value = if matches!(srf.kind, SrfKind::JsonbEachText) {
                        json::node_to_text(v).map_or(Value::Null, Value::Text)
                    } else {
                        Value::Jsonb(v.clone())
                    };
                    out.push(vec![Value::Text(k.clone()), value]);
                }
            }
            // jsonb_path_query (P2): one jsonb row per path-evaluation-sequence item. The context node
            // is already parsed above (`node`); evaluate the path (a NULL path → zero rows).
            SrfKind::JsonbPathQuery => {
                let path = srf.args[1].eval(&[], env, meter)?;
                let text = match &path {
                    Value::Null => return Ok(out),
                    Value::JsonPath(s) => s,
                    _ => unreachable!("resolver restricts the path argument to jsonpath"),
                };
                let compiled = crate::jsonpath::JsonPath::compile(text)?;
                for item in crate::jsonpath::eval(&compiled, &node)? {
                    meter.guard()?;
                    meter.charge(COSTS.generated_row);
                    out.push(vec![Value::Jsonb(item)]);
                }
            }
            // json[b]_to_record / _recordset (R1): map members → the col-def columns by name.
            SrfKind::JsonRecord { set, .. } => {
                if set {
                    let elems = match &node {
                        JsonNode::Array(e) => e,
                        _ => {
                            return Err(EngineError::new(
                                SqlState::InvalidParameterValue,
                                "cannot call json_to_recordset on a non-array",
                            ));
                        }
                    };
                    for e in elems {
                        meter.guard()?;
                        meter.charge(COSTS.generated_row);
                        out.push(json_record_row(e, &srf.record_cols, env, meter)?);
                    }
                } else {
                    meter.guard()?;
                    meter.charge(COSTS.generated_row);
                    out.push(json_record_row(&node, &srf.record_cols, env, meter)?);
                }
            }
            _ => unreachable!("json_srf_rows only handles the json SRF kinds"),
        }
        Ok(out)
    }

    pub(crate) fn unnest_rows(
        &self,
        srf: &SrfPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Vec<Row>> {
        let arr = match srf.args[0].eval(&[], env, meter)? {
            // A NULL array → zero rows (PG; the `empty_on_null` discipline).
            Value::Null => return Ok(Vec::new()),
            Value::Array(a) => a,
            _ => unreachable!("the resolver restricts unnest's argument to an array"),
        };
        let mut out: Vec<Row> = Vec::with_capacity(arr.elements.len());
        for e in arr.elements {
            meter.guard()?;
            meter.charge(COSTS.generated_row);
            out.push(vec![e]);
        }
        Ok(out)
    }
}
