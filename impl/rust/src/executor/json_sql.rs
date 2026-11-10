//! SQL/JSON evaluation: the query functions `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` with
//! their `ON EMPTY` / `ON ERROR` behaviors and `PASSING` bindings (json-sql-functions.md), the
//! record-returning functions, and `JSON_TABLE` row expansion (json-table.md).

use super::*;

/// Build one output row for `json[b]_to_record(set)` (R1) and `json[b]_populate_record(set)` (R2):
/// map each declared column to the JSON object's member of that name, coercing it to the column
/// type. A JSON null member → SQL NULL; a missing member → the base row's field (`base`, the
/// populate forms) or SQL NULL (`None`, the `_to_record` forms); a non-object node → `22023`. A
/// `json` document keeps duplicate keys, and the LAST one wins, as PostgreSQL's hash of the object
/// does (json-table.md §2). The base row is never modified.
pub(crate) fn json_record_row(
    node: &JsonNode,
    cols: &[Column],
    base: Option<&Row>,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Row> {
    let members = match node {
        JsonNode::Object(m) => m,
        _ => {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                "argument of json_to_record must be a JSON object",
            ));
        }
    };
    let mut row = Vec::with_capacity(cols.len());
    for (ci, col) in cols.iter().enumerate() {
        let member = members
            .iter()
            .rev() // last wins
            .find(|(k, _)| k == &col.name)
            .map(|(_, v)| v);
        let val = match member {
            None => match base {
                Some(b) => b[ci].clone(),
                None => Value::Null,
            },
            Some(JsonNode::Null) => Value::Null, // a JSON null overrides a base field
            Some(v) => coerce_json_member(v, &col.ty, Typmod::of_column(col), env, meter)?,
        };
        row.push(val);
    }
    Ok(row)
}

/// Coerce a JSON member node to a record column's type (R1, the JSON_VALUE scalar path): a `jsonb`
/// column embeds the node, a `json` column its canonical text, every other scalar coerces the node's
/// `->>`-style text through the cast machinery (so `"42"` / `42` → an `int` column, etc.). A
/// composite/array column type is a deferred `0A000`.
/// Resolve a SQL/JSON query function `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`
/// (json-sql-functions.md §5, S2) → an [`RExpr::JsonSqlFn`] + its fixed result type.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_json_sql_fn(
    scope: &Scope,
    kind: JsonSqlKind,
    ctx: &Expr,
    path: &Expr,
    passing: &[JsonPassing],
    returning: &Option<String>,
    wrapper: JsonWrapper,
    keep_quotes: bool,
    on_empty: Option<&JsonBehavior>,
    on_error: Option<&JsonBehavior>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // The context item — json / jsonb / text, coerced to a jsonb document at eval; a bare string
    // literal adapts to jsonb.
    let (rctx, ctx_ty) = resolve(scope, ctx, Some(ScalarType::Jsonb), agg, params)?;
    if !matches!(
        ctx_ty,
        ResolvedType::Jsonb | ResolvedType::Json | ResolvedType::Text | ResolvedType::Null
    ) {
        return Err(EngineError::new(
            SqlState::DatatypeMismatch,
            format!(
                "the context item of a SQL/JSON query function must be json/jsonb/text, not {}",
                ctx_ty.type_name()
            ),
        ));
    }
    // The path — a jsonpath; a bare string literal compiles.
    let (rpath, path_ty) = resolve(scope, path, Some(ScalarType::JsonPath), agg, params)?;
    if !matches!(path_ty, ResolvedType::JsonPath | ResolvedType::Null) {
        return Err(EngineError::new(
            SqlState::DatatypeMismatch,
            "the path of a SQL/JSON query function must be a jsonpath",
        ));
    }
    // OMIT QUOTES is the deferred S2 follow-on (the jsonb-of-bare-text result quirk).
    if !keep_quotes {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "JSON_QUERY OMIT QUOTES is not supported yet",
        ));
    }
    // The fixed RETURNING scalar type.
    let returning_st = match (kind, returning) {
        (JsonSqlKind::Exists, _) => ScalarType::Bool,
        (JsonSqlKind::Value, None) => ScalarType::Text,
        (JsonSqlKind::Query, None) => ScalarType::Jsonb,
        (_, Some(name)) => ScalarType::from_name(name).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedObject,
                format!("type \"{name}\" does not exist"),
            )
        })?,
    };
    // JSON_QUERY's result must be a JSON type (json/jsonb); JSON_VALUE's must be a scalar — a
    // composite/array RETURNING is a deferred 0A000 (it cannot hold an extracted scalar).
    if matches!(kind, JsonSqlKind::Query)
        && !matches!(returning_st, ScalarType::Json | ScalarType::Jsonb)
    {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "JSON_QUERY RETURNING a non-json type is not supported yet",
        ));
    }
    let (passing, passing_names) = resolve_json_passing(scope, passing, agg, params)?;
    let (on_empty, on_empty_default) =
        resolve_json_behavior(scope, on_empty, JsonOnBehavior::Null, returning_st, params)?;
    let on_error_fallback = match kind {
        JsonSqlKind::Exists => JsonOnBehavior::False,
        _ => JsonOnBehavior::Null,
    };
    let (on_error, on_error_default) =
        resolve_json_behavior(scope, on_error, on_error_fallback, returning_st, params)?;
    Ok((
        RExpr::JsonSqlFn {
            kind,
            ctx: Box::new(rctx),
            path: Box::new(rpath),
            passing: passing.into_boxed_slice(),
            passing_names: passing_names.into_boxed_slice(),
            returning: returning_st,
            decimal: None,
            wrapper,
            on_empty,
            on_error,
            on_empty_default: on_empty_default.map(Box::new),
            on_error_default: on_error_default.map(Box::new),
        },
        resolved_type_of(returning_st),
    ))
}

/// Resolve an `ON EMPTY` / `ON ERROR` clause → its behavior (`fallback` when the clause is absent)
/// and, for `DEFAULT expr`, the resolved expression (json-sql-functions.md §5.3). As PostgreSQL's
/// `transformJsonBehavior`: the expression must be a constant, a non-aggregate function call, or an
/// operator expression, possibly under casts (`42804` otherwise), must not reference a column
/// (`42804`), and takes the RETURNING type as its hint, so a string literal is read as that type at
/// resolve. A NULL, a value already of the RETURNING type, and a `jsonb` value (coerced as a JSON
/// item when the behavior fires) are kept; a boolean is made a JSON boolean for the same runtime
/// coercion unless the RETURNING type is `i32` (the cast); any other type is cast explicitly to the
/// RETURNING type, `42846` when no cast exists.
pub(crate) fn resolve_json_behavior(
    scope: &Scope,
    clause: Option<&JsonBehavior>,
    fallback: JsonOnBehavior,
    returning: ScalarType,
    params: &mut ParamTypes,
) -> Result<(JsonOnBehavior, Option<RExpr>)> {
    let Some(clause) = clause else {
        return Ok((fallback, None));
    };
    let Some(expr) = clause.default.as_deref() else {
        return Ok((clause.kind, None));
    };
    if !json_default_shape_ok(expr) {
        return Err(EngineError::new(
            SqlState::DatatypeMismatch,
            "can only specify a constant, non-aggregate function, or operator expression for DEFAULT",
        ));
    }
    reject_default_structure(expr).map_err(|e| {
        if e.code() == "0A000" && e.message.contains("column reference") {
            EngineError::new(
                SqlState::DatatypeMismatch,
                "DEFAULT expression must not contain column references",
            )
        } else {
            e
        }
    })?;
    let (r, ty) = resolve(scope, expr, Some(returning), &mut AggCtx::Forbidden, params)?;
    let target = resolved_type_of(returning);
    let r = match ty {
        ResolvedType::Null | ResolvedType::Jsonb => r,
        _ if ty == target => r,
        ResolvedType::Bool if returning != ScalarType::Int32 => RExpr::JsonImage {
            func: JsonImageFunc::ToJsonb,
            args: vec![r],
            ty: ResolvedType::Bool,
        },
        _ => {
            let cast = Expr::Cast {
                inner: Box::new(expr.clone()),
                type_name: returning.canonical_name().to_string(),
                type_mod: None,
                pos: SrcPos::NONE,
                cast_pos: SrcPos::NONE,
                type_pos: SrcPos::NONE,
            };
            match resolve(scope, &cast, None, &mut AggCtx::Forbidden, params) {
                Ok((r, _)) => r,
                Err(e) if e.code() == "42804" || e.code() == "0A000" => {
                    return Err(EngineError::new(
                        SqlState::CannotCoerce,
                        format!(
                            "cannot cast behavior expression of type {} to {}",
                            ty.type_name(),
                            returning.canonical_name()
                        ),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
    };
    Ok((JsonOnBehavior::Default, Some(r)))
}

/// Whether a `DEFAULT expr` behavior has the shape PostgreSQL's `ValidJsonBehaviorDefaultExpr`
/// admits: a constant, a function call, or an operator expression (not `AND`/`OR`/`NOT`, which are
/// boolean expressions there), possibly under a cast or `COLLATE`.
pub(crate) fn json_default_shape_ok(e: &Expr) -> bool {
    match e {
        Expr::Literal(..) | Expr::TypedLiteral { .. } | Expr::Like { .. } | Expr::Regex { .. } => {
            true
        }
        Expr::FuncCall {
            over, over_name, ..
        } => over.is_none() && over_name.is_none(),
        Expr::Binary { op, .. } => !matches!(op, BinaryOp::And | BinaryOp::Or),
        Expr::Unary { op, .. } => !matches!(op, UnaryOp::Not),
        Expr::Cast { inner, .. } | Expr::Collate { inner, .. } => json_default_shape_ok(inner),
        _ => false,
    }
}

/// Resolve a `PASSING value [FORMAT JSON] AS name, …` clause (json-sql-functions.md §5.4) → the
/// value expressions, each yielding a value [`passing_bindings`] reads as a jsonpath item, and the
/// names. PostgreSQL's `transformJsonPassingArgs` + `JsonItemFromDatum`: a boolean, integer,
/// `decimal`, or text value (a bare string literal is text) is the item directly, a `jsonb` value is
/// itself, and every other type is converted to `jsonb` — a `json` value parsed (last duplicate key
/// wins), anything else by `to_jsonb`; a float is the number `float8_numeric`/`float4_numeric`
/// makes of it. `FORMAT JSON` parses a text value as JSON instead (`22P02` on malformed text) and
/// converts every other type by `to_jsonb`, except a `bytea` value: that is UTF-8 encoded JSON
/// text, left as `bytea` here and decoded by [`passing_bindings`]. `ENCODING` is only allowed on a
/// `bytea` value (`42804`), and only `UTF8` is supported (`22023`, PostgreSQL's conversion error).
/// `0A000`: a `date`/`timestamp`/`timestamptz` value, which PostgreSQL passes as a jsonpath datetime
/// item (jed has none, by decision, jsonpath.md §3.1); and, deferred, a NaN or ±Infinity float (at
/// evaluation), which PostgreSQL passes as a numeric NaN/±Infinity that jed's `decimal` cannot hold.
pub(crate) fn resolve_json_passing(
    scope: &Scope,
    passing: &[JsonPassing],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(Vec<RExpr>, Vec<String>)> {
    let mut exprs = Vec::with_capacity(passing.len());
    let mut names = Vec::with_capacity(passing.len());
    for p in passing {
        let hint = if matches!(
            p.expr,
            Expr::Literal(Literal::Text(_), _) | Expr::Param(_, _)
        ) {
            Some(ScalarType::Text)
        } else {
            None
        };
        let (r, ty) = resolve(scope, &p.expr, hint, agg, params)?;
        let unsupported = |what: &str| {
            Err(EngineError::new(
                SqlState::FeatureNotSupported,
                format!("a {what} PASSING value is not supported yet"),
            ))
        };
        let to_jsonb = |r: RExpr, ty: ResolvedType, params: &mut ParamTypes| {
            params.nonimmutable |= ty.contains_timestamptz();
            RExpr::JsonImage {
                func: JsonImageFunc::ToJsonb,
                args: vec![r],
                ty,
            }
        };
        let cast_jsonb = |r: RExpr| RExpr::Cast {
            inner: Box::new(r),
            target: ScalarType::Jsonb,
            typmod: None,
            varchar_len: None,
            trap: TrapClass::May,
            temporal: None,
        };
        if let Some(enc) = &p.encoding {
            if !matches!(ty, ResolvedType::Bytea) {
                return Err(EngineError::new(
                    SqlState::DatatypeMismatch,
                    "JSON ENCODING clause is only allowed for bytea input type",
                ));
            }
            if enc != "UTF8" {
                return Err(EngineError::new(
                    SqlState::InvalidParameterValue,
                    format!("invalid source encoding name \"{enc}\""),
                ));
            }
        }
        let value = match (&ty, p.format_json) {
            (ResolvedType::Bytea, true) => r,
            (ResolvedType::Null | ResolvedType::Jsonb, _) => r,
            (ResolvedType::Json, _) | (ResolvedType::Text, true) => cast_jsonb(r),
            (_, true) => to_jsonb(r, ty, params),
            (
                ResolvedType::Bool
                | ResolvedType::Int(_)
                | ResolvedType::Decimal
                | ResolvedType::Float(_)
                | ResolvedType::Text,
                false,
            ) => r,
            (ResolvedType::Date | ResolvedType::Timestamp | ResolvedType::Timestamptz, false) => {
                return unsupported("date/time");
            }
            (_, false) => to_jsonb(r, ty, params),
        };
        exprs.push(value);
        names.push(p.name.clone());
    }
    Ok((exprs, names))
}

/// Evaluate a resolved `PASSING` clause into its jsonpath variables (json-sql-functions.md §5.4):
/// each value as a `jsonb` item (SQL NULL is JSON `null`), ordered for lookup by
/// [`crate::jsonpath::passing_vars`].
pub(crate) fn passing_bindings(
    exprs: &[RExpr],
    names: &[String],
    row: &[Value],
    env: &EvalEnv,
    m: &mut Meter,
) -> Result<Vec<crate::jsonpath::PassingVar>> {
    let mut bindings = Vec::with_capacity(exprs.len());
    for (e, name) in exprs.iter().zip(names) {
        let node = match e.eval(row, env, m)? {
            Value::Null => JsonNode::Null,
            Value::Bool(b) => JsonNode::Bool(b),
            Value::Int(n) => JsonNode::Number(Decimal::from_i64(n)),
            Value::Decimal(d) => JsonNode::Number(d),
            Value::Float32(f) if f.is_finite() => {
                JsonNode::Number(Decimal::from_float32_flt_dig(f))
            }
            Value::Float64(f) if f.is_finite() => {
                JsonNode::Number(Decimal::from_float64_dbl_dig(f))
            }
            Value::Float32(_) | Value::Float64(_) => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a NaN or infinite float PASSING value is not supported yet",
                ));
            }
            Value::Text(s) => JsonNode::String(s),
            Value::Jsonb(n) => n,
            // `FORMAT JSON` on a `bytea` value: UTF-8 encoded JSON text, parsed as the text cast does.
            Value::Bytea(b) => {
                m.charge(COSTS.operator_eval);
                json::jsonb_in(&decode_utf8_text(&b)?)?
            }
            _ => unreachable!("resolve_json_passing converts every other type to jsonb"),
        };
        bindings.push((name.clone(), node));
    }
    Ok(crate::jsonpath::passing_vars(bindings))
}

/// Decode UTF-8 bytes as text, as PostgreSQL's `convert_from(…, 'UTF8')` verifies them: an invalid
/// sequence or a NUL byte is `22021`, naming the sequence's bytes (up to its expected length, at most
/// what remains) as PostgreSQL's `report_invalid_encoding` does.
pub(crate) fn decode_utf8_text(b: &[u8]) -> Result<String> {
    let bad = match std::str::from_utf8(b) {
        Ok(s) => match s.find('\0') {
            None => return Ok(s.to_string()),
            Some(i) => i,
        },
        Err(e) => {
            let valid = &b[..e.valid_up_to()];
            valid
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.valid_up_to())
        }
    };
    let lead = b[bad];
    let want = match lead {
        0x00..=0x7f => 1,
        _ if lead & 0xe0 == 0xc0 => 2,
        _ if lead & 0xf0 == 0xe0 => 3,
        _ if lead & 0xf8 == 0xf0 => 4,
        _ => 1,
    };
    let seq = &b[bad..(bad + want).min(b.len())];
    let detail = seq
        .iter()
        .map(|c| format!("0x{c:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    Err(EngineError::new(
        SqlState::CharacterNotInRepertoire,
        format!("invalid byte sequence for encoding \"UTF8\": {detail}"),
    ))
}

/// A SQL/JSON error that the query functions' `ON ERROR` clause catches: a data exception (class
/// `22`). Resource / cost aborts (class `53`/`54`) propagate unconditionally.
pub(crate) fn is_sqljson_error(e: &EngineError) -> bool {
    e.code().starts_with("22")
}

/// The `DEFAULT expr` expressions of a SQL/JSON query function's `ON EMPTY` / `ON ERROR` clauses
/// and the row they are evaluated against (json-sql-functions.md §5.3).
#[derive(Clone, Copy)]
pub(crate) struct JsonDefaults<'a> {
    pub(crate) on_empty: Option<&'a RExpr>,
    pub(crate) on_error: Option<&'a RExpr>,
    pub(crate) row: &'a [Value],
}

/// Which clause a behavior answers: `ON EMPTY` or `ON ERROR` (named in a DEFAULT coercion error).
#[derive(Clone, Copy)]
pub(crate) enum JsonClause {
    Empty,
    Error,
}

/// Apply an `ON ERROR` / `ON EMPTY` behavior → a value of the RETURNING type. `underlying`
/// is the SQL/JSON error this behavior replaces (raised verbatim by `ERROR`). A `DEFAULT expr` is
/// evaluated here, outside the clause: its own errors propagate, as in PostgreSQL. Its value is
/// already of the RETURNING type, except a `jsonb` value, which is coerced as a JSON item (PostgreSQL's
/// runtime `json_populate_type` coercion); a failure there is `42804`.
pub(crate) fn apply_json_behavior(
    behavior: JsonOnBehavior,
    clause: JsonClause,
    defaults: JsonDefaults<'_>,
    underlying: EngineError,
    returning: ScalarType,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    match behavior {
        JsonOnBehavior::Default => {
            let (expr, name) = match clause {
                JsonClause::Empty => (defaults.on_empty, "EMPTY"),
                JsonClause::Error => (defaults.on_error, "ERROR"),
            };
            let expr = expr.expect("a DEFAULT behavior carries its expression");
            match expr.eval(defaults.row, env, meter)? {
                Value::Jsonb(node) if !returning.is_jsonb() => {
                    coerce_json_member(&node, &Type::Scalar(returning), Typmod::NONE, env, meter)
                        .map_err(|e| {
                            if !is_sqljson_error(&e) {
                                return e;
                            }
                            EngineError::new(
                        SqlState::DatatypeMismatch,
                        format!(
                            "could not coerce ON {name} expression (DEFAULT) to the RETURNING type"
                        ),
                    )
                    .with_detail(Some(e.message.clone()))
                        })
                }
                v => Ok(v),
            }
        }
        JsonOnBehavior::Error => Err(underlying),
        JsonOnBehavior::Null => Ok(Value::Null),
        JsonOnBehavior::True => Ok(Value::Bool(true)),
        JsonOnBehavior::False => Ok(Value::Bool(false)),
        JsonOnBehavior::Unknown => Ok(Value::Null),
        JsonOnBehavior::EmptyArray => {
            json_node_as_returning(JsonNode::Array(Vec::new()), returning, env, meter)
        }
        JsonOnBehavior::EmptyObject => {
            json_node_as_returning(JsonNode::Object(Vec::new()), returning, env, meter)
        }
    }
}

/// Render a json result node as the RETURNING type: `jsonb` embeds, `json` its canonical text, any
/// other scalar coerces the node's `->>`-style text through the cast machinery.
pub(crate) fn json_node_as_returning(
    node: JsonNode,
    returning: ScalarType,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    coerce_json_member(&node, &Type::Scalar(returning), Typmod::NONE, env, meter)
}

/// Apply the SQL/JSON query-function semantics (JSON_VALUE / JSON_QUERY) to an evaluated sequence.
/// (JSON_EXISTS is handled inline — non-empty → true.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn eval_json_sql_result(
    kind: JsonSqlKind,
    seq: Vec<Cow<'_, JsonNode>>,
    returning: ScalarType,
    decimal: Option<DecimalTypmod>,
    wrapper: JsonWrapper,
    on_empty: JsonOnBehavior,
    on_error: JsonOnBehavior,
    defaults: JsonDefaults<'_>,
    constant: bool,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    match kind {
        JsonSqlKind::Exists => Ok(Value::Bool(!seq.is_empty())),
        JsonSqlKind::Value => {
            if seq.is_empty() {
                return apply_json_behavior(
                    on_empty,
                    JsonClause::Empty,
                    defaults,
                    EngineError::new(SqlState::NoSqlJsonItem, "no SQL/JSON item"),
                    returning,
                    env,
                    meter,
                );
            }
            if seq.len() > 1 {
                return apply_json_behavior(
                    on_error,
                    JsonClause::Error,
                    defaults,
                    EngineError::new(
                        SqlState::MoreThanOneSqlJsonItem,
                        "JSON path expression in JSON_VALUE should return singleton scalar item",
                    ),
                    returning,
                    env,
                    meter,
                );
            }
            let item = seq[0].as_ref();
            // JSON_VALUE requires a SCALAR item (PG 2203F otherwise).
            if matches!(item, JsonNode::Array(_) | JsonNode::Object(_)) {
                return apply_json_behavior(
                    on_error,
                    JsonClause::Error,
                    defaults,
                    EngineError::new(
                        SqlState::SqlJsonScalarRequired,
                        "JSON path expression in JSON_VALUE should return singleton scalar item",
                    ),
                    returning,
                    env,
                    meter,
                );
            }
            // Coerce the scalar to the RETURNING type (a JSON null → SQL NULL). A coercion failure is
            // a SQL/JSON error honored by ON ERROR.
            let tm = Typmod {
                decimal,
                ..Typmod::NONE
            };
            match coerce_json_member(item, &Type::Scalar(returning), tm, env, meter) {
                Ok(v) => Ok(v),
                Err(e) if is_sqljson_error(&e) => apply_json_behavior(
                    on_error,
                    JsonClause::Error,
                    defaults,
                    e,
                    returning,
                    env,
                    meter,
                ),
                Err(e) => Err(e),
            }
        }
        JsonSqlKind::Query => {
            // Whether a wrapper built an array: then JSON_QUERY is a growth kernel (cost.md §8.1).
            let wrapped = match wrapper {
                JsonWrapper::Unconditional => true,
                JsonWrapper::Conditional => seq.len() != 1,
                JsonWrapper::Without => false,
            };
            let array = |seq: Vec<Cow<JsonNode>>| {
                JsonNode::Array(seq.into_iter().map(Cow::into_owned).collect())
            };
            let node = match wrapper {
                JsonWrapper::Unconditional => array(seq),
                JsonWrapper::Conditional => {
                    if seq.len() == 1 {
                        seq.into_iter().next().unwrap().into_owned()
                    } else {
                        array(seq)
                    }
                }
                JsonWrapper::Without => {
                    if seq.is_empty() {
                        return apply_json_behavior(
                            on_empty,
                            JsonClause::Empty,
                            defaults,
                            EngineError::new(SqlState::NoSqlJsonItem, "no SQL/JSON item"),
                            returning,
                            env,
                            meter,
                        );
                    }
                    if seq.len() > 1 {
                        return apply_json_behavior(
                            on_error,
                            JsonClause::Error,
                            defaults,
                            EngineError::new(
                                SqlState::MoreThanOneSqlJsonItem,
                                "JSON path expression in JSON_QUERY should return singleton item without wrapper",
                            ),
                            returning,
                            env,
                            meter,
                        );
                    }
                    seq.into_iter().next().unwrap().into_owned()
                }
            };
            // A wrapper nests the items one level (json.md §6.4) — not a SQL/JSON error, so it is
            // raised regardless of ON ERROR.
            json::check_depth(&node)?;
            let out = json_node_as_returning(node, returning, env, meter)?;
            if wrapped && !constant {
                charge_output(meter, &out)?;
            }
            Ok(out)
        }
    }
}

pub(crate) fn coerce_json_member(
    node: &JsonNode,
    col_ty: &Type,
    tm: Typmod,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    match col_ty {
        Type::Scalar(ScalarType::Jsonb) => Ok(Value::Jsonb(node.clone())),
        Type::Scalar(ScalarType::Json) => Ok(Value::Json(json::jsonb_out(node))),
        Type::Scalar(st) => match json::node_to_text(node) {
            None => Ok(Value::Null),
            Some(text) => {
                let (rexpr, _) = coerce_string_literal(&text, *st, tm, &env.exec.input_zone())?;
                rexpr.eval(&[], env, meter)
            }
        },
        _ => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "a composite/array record column is not supported yet",
        )),
    }
}

// ----------------------------------------------------------------------------------------------
// JSON_TABLE helpers (T1, json-table.md §3)
// ----------------------------------------------------------------------------------------------

/// A sparse assignment of a `JSON_TABLE` row — `(flat column index, value)` pairs; unassigned
/// columns are NULL (the LEFT-OUTER / sibling-UNION fill).
pub(crate) type JtAssign = Vec<(usize, Value)>;

/// Build a synthetic `JSON_TABLE` output column.
pub(crate) fn jt_column(name: &str, ty: ScalarType, decimal: Option<DecimalTypmod>) -> Column {
    Column {
        name: name.to_string(),
        ty: Type::Scalar(ty),
        decimal,
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
    }
}

/// Resolve a `JSON_TABLE` column type name → its scalar type (a composite → `0A000`, an unknown
/// name → `42704`).
pub(crate) fn jt_scalar_type(db: &Engine, type_name: &str) -> Result<ScalarType> {
    if let Some(st) = ScalarType::from_name(type_name) {
        Ok(st)
    } else if db.composite_type(type_name).is_some() {
        Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "a composite JSON_TABLE column is not supported yet",
        ))
    } else {
        Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("type \"{type_name}\" does not exist"),
        ))
    }
}

/// Compile a `JSON_TABLE` column path — the explicit `PATH p`, or the default `$."<column_name>"` —
/// to its canonical rendered form (validating; malformed → 42601). The default member name is a
/// QUOTED jsonpath string (JSON-escaped), as PostgreSQL builds it, so a column named `a.b`, `a b`,
/// or `1` reads the member of exactly that name (json-table.md §3.1).
pub(crate) fn jt_compile_path(
    path: Option<&str>,
    name: &str,
    casing: Option<&crate::collation::PropertyTable>,
) -> Result<String> {
    let src = match path {
        Some(p) => p.to_string(),
        None => {
            let mut s = String::from("$.");
            json::write_json_string(name, &mut s);
            s
        }
    };
    Ok(crate::jsonpath::JsonPath::compile(&src, casing)?.render())
}

/// How `JSON_TABLE` evaluates its row, NESTED, and value-column paths: the full sequence, with its
/// `PASSING` variables (jsonpath.md §3.1, §5), folding an `i`-flagged `like_regex` under `casing`.
pub(crate) fn jt_eval<'d>(
    vars: crate::jsonpath::Vars<'d>,
    casing: Option<&'d crate::collation::PropertyTable>,
) -> crate::jsonpath::EvalOpts<'d> {
    crate::jsonpath::EvalOpts {
        exists: false,
        vars,
        casing,
    }
}

/// Expand a `JSON_TABLE` COLUMNS level over a sequence of row items → the sparse rows (the
/// parent→child LEFT OUTER product with sibling NESTED paths UNIONed, json-table.md §3.3). Every
/// path reads the `PASSING` variables `vars`.
pub(crate) fn expand_jt_level(
    cols: &[JtCol],
    items: &[Cow<'_, JsonNode>],
    vars: crate::jsonpath::Vars<'_>,
    defaults: &[RExpr],
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Vec<JtAssign>> {
    let mut rows: Vec<JtAssign> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        meter.checkpoint()?;
        let ord = (i + 1) as i64;
        // This level's non-nested columns (regular / exists / ordinality).
        let mut local: JtAssign = Vec::new();
        for col in cols {
            match col {
                JtCol::Ordinality { idx } => local.push((*idx, Value::Int(ord))),
                JtCol::Regular {
                    idx,
                    returning,
                    decimal,
                    path,
                    query,
                    wrapper,
                    on_empty,
                    on_error,
                    on_empty_default,
                    on_error_default,
                } => {
                    let jd = JsonDefaults {
                        on_empty: on_empty_default.map(|i| &defaults[i]),
                        on_error: on_error_default.map(|i| &defaults[i]),
                        row: &[],
                    };
                    let v = eval_jt_regular(
                        item, path, vars, *query, *returning, *decimal, *wrapper, *on_empty,
                        *on_error, jd, env, meter,
                    )?;
                    local.push((*idx, v));
                }
                JtCol::Exists {
                    idx,
                    returning,
                    path,
                    on_error,
                } => {
                    let casing = env.exec.session.env.property();
                    let v = eval_jt_exists(item, path, vars, casing, *returning, *on_error, meter)?;
                    local.push((*idx, v));
                }
                JtCol::Nested { .. } => {}
            }
        }
        // The NESTED siblings, expanded over this item (UNIONed + LEFT OUTER fill).
        let nested: Vec<&JtCol> = cols
            .iter()
            .filter(|c| matches!(c, JtCol::Nested { .. }))
            .collect();
        let nested_rows = expand_jt_nested(&nested, item, vars, defaults, env, meter)?;
        for nr in nested_rows {
            let mut row = local.clone();
            row.extend(nr);
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Expand the NESTED siblings of a level over one parent item — the default-plan **UNION** of the
/// siblings (each row fills only its own subtree), with the parent→child **LEFT OUTER** fill (no
/// child rows at all → one all-NULL nested row).
pub(crate) fn expand_jt_nested(
    children: &[&JtCol],
    item: &JsonNode,
    vars: crate::jsonpath::Vars<'_>,
    defaults: &[RExpr],
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Vec<JtAssign>> {
    if children.is_empty() {
        return Ok(vec![Vec::new()]);
    }
    let mut union: Vec<JtAssign> = Vec::new();
    for child in children {
        if let JtCol::Nested { path, columns } = child {
            let casing = env.exec.session.env.property();
            let p = crate::jsonpath::compile_metered(path, casing, meter)?;
            // A navigation error yields no child rows; a cost abort propagates (jsonpath.md §7).
            let child_seq = match crate::jsonpath::eval(&p, item, jt_eval(vars, casing), meter) {
                Ok(seq) => seq,
                Err(e) if is_sqljson_error(&e) => Vec::new(),
                Err(e) => return Err(e),
            };
            union.extend(expand_jt_level(
                columns, &child_seq, vars, defaults, env, meter,
            )?);
        }
    }
    if union.is_empty() {
        union.push(Vec::new());
    }
    Ok(union)
}

/// Evaluate a regular `JSON_TABLE` column over a row item — JSON_VALUE (scalar) / JSON_QUERY
/// (json/jsonb) semantics, with the column's wrapper / ON EMPTY / ON ERROR.
#[allow(clippy::too_many_arguments)]
pub(crate) fn eval_jt_regular(
    item: &JsonNode,
    path: &str,
    vars: crate::jsonpath::Vars<'_>,
    query: bool,
    returning: ScalarType,
    decimal: Option<DecimalTypmod>,
    wrapper: JsonWrapper,
    on_empty: JsonOnBehavior,
    on_error: JsonOnBehavior,
    defaults: JsonDefaults<'_>,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    let casing = env.exec.session.env.property();
    let p = crate::jsonpath::compile_metered(path, casing, meter)?;
    let seq = match crate::jsonpath::eval(&p, item, jt_eval(vars, casing), meter) {
        Ok(s) => s,
        Err(e) if is_sqljson_error(&e) => {
            return apply_json_behavior(
                on_error,
                JsonClause::Error,
                defaults,
                e,
                returning,
                env,
                meter,
            );
        }
        Err(e) => return Err(e),
    };
    let kind = if query {
        JsonSqlKind::Query
    } else {
        JsonSqlKind::Value
    };
    // A JSON_TABLE row item is never a constant operand, so a wrapped column charges its array.
    eval_json_sql_result(
        kind, seq, returning, decimal, wrapper, on_empty, on_error, defaults, false, env, meter,
    )
}

/// Evaluate an `EXISTS` `JSON_TABLE` column over a row item — JSON_EXISTS, coerced to the column
/// type (a NON-empty sequence is true; a structural error honors ON ERROR, default FALSE).
pub(crate) fn eval_jt_exists(
    item: &JsonNode,
    path: &str,
    vars: crate::jsonpath::Vars<'_>,
    casing: Option<&crate::collation::PropertyTable>,
    returning: ScalarType,
    on_error: JsonOnBehavior,
    meter: &mut Meter,
) -> Result<Value> {
    let p = crate::jsonpath::compile_metered(path, casing, meter)?;
    let opts = crate::jsonpath::EvalOpts {
        exists: true,
        vars,
        casing,
    };
    let exists = match crate::jsonpath::eval(&p, item, opts, meter) {
        Ok(seq) => !seq.is_empty(),
        Err(e) if is_sqljson_error(&e) => match on_error {
            JsonOnBehavior::Error => return Err(e),
            JsonOnBehavior::True => true,
            JsonOnBehavior::Unknown => return Ok(Value::Null),
            _ => false,
        },
        Err(e) => return Err(e),
    };
    // Coerce the boolean to the column type (a `boolean` column → bool; an integer column → 1/0).
    if returning.is_bool() {
        Ok(Value::Bool(exists))
    } else if returning.is_integer() {
        Ok(Value::Int(i64::from(exists)))
    } else {
        Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "an EXISTS JSON_TABLE column must be boolean or integer this slice",
        ))
    }
}
