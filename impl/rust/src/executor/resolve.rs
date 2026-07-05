//! Binary-operator and operand-pair resolution (mirrors part of impl/go resolve.go): resolve_binary and
//! the operand-pair/int-or-decimal resolution helpers, the comparability classification, and the numeric
//! gcd/lcm helpers used to resolve arithmetic/comparison operators.

use super::*;

pub(crate) fn resolve_binary(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            // jsonb `-` is the delete operator (json-sql-functions.md §1, J6), NOT arithmetic — its
            // right operand is a key/index/keys, never an arithmetic value. Peek the LHS type; a
            // jsonb LHS with `-` routes to the delete resolver. (Only `-` has a jsonb meaning; `+ *
            // / %` over a jsonb operand fall through and 42804 in the numeric path.)
            if matches!(op, BinaryOp::Sub) {
                let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
                if matches!(lt, ResolvedType::Jsonb) {
                    return resolve_jsonb_delete(scope, false, lhs, rhs, rl, agg, params);
                }
            }
            // Arithmetic is overloaded across integer and decimal. Resolve the operand pair
            // (an integer literal adapts to an integer sibling), then pick the family: both
            // integer → integer arithmetic (promotion tower); at least one decimal → decimal
            // arithmetic (the integer operand widens at eval); a text/boolean operand is a
            // 42804 (spec/design/decimal.md §4).
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            // Range set operators (RF4, spec/design/range-functions.md §4): `+` union, `-`
            // difference, `*` intersection over two ranges. A range operand in any of these three is
            // the set-op axis — both operands must be ranges of a common element type, else 42883
            // (matching PG's "operator does not exist"); the numeric/temporal arithmetic below never
            // sees a range. `/` and `%` have no range meaning and fall straight through.
            if matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul)
                && (matches!(lt, ResolvedType::Range(_)) || matches!(rt, ResolvedType::Range(_)))
            {
                return resolve_range_set_op(op, rl, lt, rr, rt);
            }
            // Date arithmetic (spec/design/date.md §6): date ± int → date, date − date → i32
            // (days between), date ± interval → timestamp. Checked BEFORE the interval/timestamp
            // rules below: a `date ± interval` pair has an interval operand, which would otherwise
            // make `temporal_arith_result` report a 42804 (date is not one of its temporal types).
            // Any other arithmetic combination involving a date is a 42804 from `date_arith_result`.
            if matches!(lt, ResolvedType::Date) || matches!(rt, ResolvedType::Date) {
                let result = date_arith_result(op, &lt, &rt)?;
                let aop = if matches!(op, BinaryOp::Add) {
                    ArithOp::Add
                } else {
                    ArithOp::Sub
                };
                return Ok((
                    RExpr::Arith {
                        op: aop,
                        lhs: Box::new(rl),
                        rhs: Box::new(rr),
                        result,
                    },
                    resolved_type_of(result),
                ));
            }
            // interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5).
            // interval * number, number * interval (commute), interval / number. Checked before
            // the ±-only temporal rule below.
            if let Some(res) = interval_scale_result(op, &lt, &rt) {
                let result = res?;
                let aop = if matches!(op, BinaryOp::Mul) {
                    ArithOp::Mul
                } else {
                    ArithOp::Div
                };
                return Ok((
                    RExpr::Arith {
                        op: aop,
                        lhs: Box::new(rl),
                        rhs: Box::new(rr),
                        result,
                    },
                    resolved_type_of(result),
                ));
            }
            // Temporal arithmetic (spec/design/interval.md §5): interval ± interval, timestamp[tz]
            // ± interval, interval + timestamp[tz], and timestamp[tz] − timestamp[tz] → interval.
            // The eval dispatches on the value kinds; here we settle the result type. A temporal
            // operand in any other combination is a 42804.
            if let Some(res) = temporal_arith_result(op, &lt, &rt) {
                let result = res?;
                let aop = if matches!(op, BinaryOp::Add) {
                    ArithOp::Add
                } else {
                    ArithOp::Sub
                };
                return Ok((
                    RExpr::Arith {
                        op: aop,
                        lhs: Box::new(rl),
                        rhs: Box::new(rr),
                        result,
                    },
                    resolved_type_of(result),
                ));
            }
            // Float arithmetic (spec/design/float.md §5): float ⊕ float → float, mixed widths
            // PROMOTE to f64 first (the implicit f32 → f64 cast). A float paired with
            // any non-float family is a 42804 (the strict island), reported by require_numeric
            // below since one side is Float. A pure float pair (or float × NULL) is handled here.
            if matches!(lt, ResolvedType::Float(_)) || matches!(rt, ResolvedType::Float(_)) {
                match promote_float_arith(rl, lt, rr, rt) {
                    Some((rl, rr, result)) => {
                        let aop = match op {
                            BinaryOp::Add => ArithOp::Add,
                            BinaryOp::Sub => ArithOp::Sub,
                            BinaryOp::Mul => ArithOp::Mul,
                            BinaryOp::Div => ArithOp::Div,
                            BinaryOp::Mod => ArithOp::Mod,
                            _ => unreachable!(),
                        };
                        return Ok((
                            RExpr::Arith {
                                op: aop,
                                lhs: Box::new(rl),
                                rhs: Box::new(rr),
                                result,
                            },
                            ResolvedType::Float(result),
                        ));
                    }
                    // A float paired with a non-float, non-NULL family — the strict island
                    // (int/decimal × float is 42804, spec/design/float.md §6).
                    None => {
                        return Err(type_error("arithmetic operators require numeric operands"));
                    }
                }
            }
            require_numeric_operand(&lt)?;
            require_numeric_operand(&rt)?;
            let aop = match op {
                BinaryOp::Add => ArithOp::Add,
                BinaryOp::Sub => ArithOp::Sub,
                BinaryOp::Mul => ArithOp::Mul,
                BinaryOp::Div => ArithOp::Div,
                BinaryOp::Mod => ArithOp::Mod,
                _ => unreachable!(),
            };
            let (result, rty) = if lt == ResolvedType::Decimal || rt == ResolvedType::Decimal {
                (ScalarType::Decimal, ResolvedType::Decimal)
            } else {
                let p = promote(&lt, &rt);
                (p, ResolvedType::Int(p))
            };
            Ok((
                RExpr::Arith {
                    op: aop,
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    result,
                },
                rty,
            ))
        }
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            // Comparison is overloaded across families: integer×integer or text×text.
            // Resolve the operands (a literal adapts to its sibling; text literals stay
            // text), then require they be comparable — a mixed integer/text pair is 42804.
            // The runtime comparison (eq3/lt3/gt3) dispatches on the value variants.
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            classify_comparable(&lt, &rt)?;
            // A mixed-width float comparison promotes the f32 side to f64 first (the
            // implicit cast — spec/design/float.md §2/§3), so the runtime compare sees one width.
            let (rl, rr) =
                if matches!(lt, ResolvedType::Float(_)) && matches!(rt, ResolvedType::Float(_)) {
                    (widen_float_to_f64(rl, &lt), widen_float_to_f64(rr, &rt))
                } else {
                    (rl, rr)
                };
            let cop = match op {
                BinaryOp::Eq => CmpOp::Eq,
                BinaryOp::Ne => CmpOp::Ne,
                BinaryOp::Lt => CmpOp::Lt,
                BinaryOp::Gt => CmpOp::Gt,
                BinaryOp::Le => CmpOp::Le,
                BinaryOp::Ge => CmpOp::Ge,
                _ => unreachable!(),
            };
            // Derive the comparison's collation (spec/design/collation.md §1/§7). Only a text×text
            // comparison is collatable; for any other operand family collation is irrelevant (and a
            // COLLATE on a non-text operand was already rejected 42804 at the Collate node). Each
            // operand's derivation (explicit COLLATE / implicit column collation / none) is combined
            // per PG's rules: two different EXPLICIT collations conflict (42P21); two different
            // IMPLICIT collations are indeterminate (42P22 when consumed here). The derivation runs
            // for ALL comparison ops including `=`/`<>` (PG raises the conflict regardless), even
            // though `=`/`<>` ignore the collation at eval (byte equality, §7).
            let collation = if matches!(lt, ResolvedType::Text) && matches!(rt, ResolvedType::Text)
            {
                let d =
                    combine_deriv(derive_collation(scope, lhs)?, derive_collation(scope, rhs)?)?;
                resolve_deriv(scope.catalog, d)?
            } else {
                None
            };
            Ok((
                RExpr::Compare {
                    op: cop,
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    collation,
                },
                ResolvedType::Bool,
            ))
        }
        BinaryOp::And | BinaryOp::Or => {
            let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
            let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
            require_bool(&lt, "AND/OR requires boolean operands")?;
            require_bool(&rt, "AND/OR requires boolean operands")?;
            let node = if matches!(op, BinaryOp::And) {
                RExpr::And(Box::new(rl), Box::new(rr))
            } else {
                RExpr::Or(Box::new(rl), Box::new(rr))
            };
            Ok((node, ResolvedType::Bool))
        }
        BinaryOp::Concat => resolve_concat(scope, lhs, rhs, agg, params),
        // The containment/overlap operators (@>/<@/&&, shared by arrays and ranges) and the five
        // range-only positional/adjacency operators (<</>>/&</&>/-|-) all dispatch here: the operand
        // type chooses the array axis (array-functions.md §10) or the range axis (range-functions.md §3).
        BinaryOp::Contains
        | BinaryOp::ContainedBy
        | BinaryOp::Overlaps
        | BinaryOp::StrictlyLeft
        | BinaryOp::StrictlyRight
        | BinaryOp::NotExtendRight
        | BinaryOp::NotExtendLeft
        | BinaryOp::Adjacent => resolve_set_op(scope, op, lhs, rhs, agg, params),
        // The jsonb accessor operators (spec/design/json-sql-functions.md §1, J4).
        BinaryOp::JsonGet
        | BinaryOp::JsonGetText
        | BinaryOp::JsonGetPath
        | BinaryOp::JsonGetPathText => resolve_json_access(scope, op, lhs, rhs, agg, params),
        // The jsonb key-existence operators (spec/design/json-sql-functions.md §1, J5).
        BinaryOp::JsonHasKey => resolve_json_has_key(scope, HasKeyKind::One, lhs, rhs, agg, params),
        BinaryOp::JsonHasAnyKey => {
            resolve_json_has_key(scope, HasKeyKind::Any, lhs, rhs, agg, params)
        }
        BinaryOp::JsonHasAllKeys => {
            resolve_json_has_key(scope, HasKeyKind::All, lhs, rhs, agg, params)
        }
        // The jsonb delete-at-path operator `#-` (spec/design/json-sql-functions.md §1, J6). `||`
        // and `-` (delete) are dispatched by operand type in resolve_concat / the arithmetic arm.
        BinaryOp::JsonDeletePath => {
            let (rbase, base_ty) = resolve(scope, lhs, Some(ScalarType::Jsonb), agg, params)?;
            match base_ty {
                ResolvedType::Jsonb | ResolvedType::Null => {}
                _ => {
                    return Err(EngineError::new(
                        SqlState::UndefinedFunction,
                        format!("operator does not exist: {} #- text[]", base_ty.type_name()),
                    ));
                }
            }
            resolve_jsonb_delete(scope, true, lhs, rhs, rbase, agg, params)
        }
        // `jsonb @? jsonpath` = jsonb_path_exists, `jsonb @@ jsonpath` = jsonb_path_match
        // (jsonpath.md §6). Both reuse the jsonpath kernels.
        BinaryOp::JsonPathExists | BinaryOp::JsonPathMatch => {
            let (sym, kind) = if matches!(op, BinaryOp::JsonPathExists) {
                ("@?", JsonPathFnKind::Exists)
            } else {
                ("@@", JsonPathFnKind::Match)
            };
            let (ctx, ct) = resolve(scope, lhs, Some(ScalarType::Jsonb), agg, params)?;
            if !matches!(ct, ResolvedType::Jsonb | ResolvedType::Null) {
                return Err(EngineError::new(
                    SqlState::UndefinedFunction,
                    format!("operator does not exist: {} {sym} jsonpath", ct.type_name()),
                ));
            }
            let (path, pt) = resolve(scope, rhs, Some(ScalarType::JsonPath), agg, params)?;
            if !matches!(pt, ResolvedType::JsonPath | ResolvedType::Null) {
                return Err(EngineError::new(
                    SqlState::UndefinedFunction,
                    format!("operator does not exist: jsonb {sym} (a non-jsonpath)"),
                ));
            }
            Ok((
                RExpr::JsonPathFn {
                    kind,
                    args: vec![ctx, path],
                },
                ResolvedType::Bool,
            ))
        }
    }
}

/// Resolve a jsonb accessor operator (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1). The
/// base must be `jsonb` (a `json` base is the deferred 0A000 follow-on — json.md §4; any other base
/// is 42883). For `->`/`->>` the argument is a key (`text`) or an array index (`integer`); for
/// `#>`/`#>>` it is a `text[]` path (a bare string literal `'{a,b}'` adapts via `array_in`). The
/// result is `jsonb` (`-> #>`) or `text` (`->> #>>`); a missing access yields SQL NULL at eval.
pub(crate) fn resolve_json_access(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let (rbase, base_ty) = resolve(scope, lhs, None, agg, params)?;
    // The base must be jsonb. json is a documented deferred follow-on (its operators preserve the
    // verbatim sub-text — json.md §4); any other base type has no such operator (42883).
    match base_ty {
        ResolvedType::Jsonb => {}
        ResolvedType::Json => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "json accessor operators are not supported yet; cast to jsonb",
            ));
        }
        ResolvedType::Null => {} // a NULL base propagates (the access is NULL)
        _ => {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!(
                    "operator does not exist: {} {} ...",
                    base_ty.type_name(),
                    json_op_symbol(op)
                ),
            ));
        }
    }
    let (jop, result, path) = match op {
        BinaryOp::JsonGet => (JsonGetOp::Arrow, ResolvedType::Jsonb, false),
        BinaryOp::JsonGetText => (JsonGetOp::ArrowText, ResolvedType::Text, false),
        BinaryOp::JsonGetPath => (JsonGetOp::HashArrow, ResolvedType::Jsonb, true),
        BinaryOp::JsonGetPathText => (JsonGetOp::HashArrowText, ResolvedType::Text, true),
        _ => unreachable!("resolve_json_access only handles the four accessor operators"),
    };
    let rarg = if path {
        // `#>` / `#>>` take a text[] path. A bare string literal `'{a,b}'` adapts via array_in;
        // otherwise the resolved argument must be a text[] (else 42883).
        if let Expr::Literal(Literal::Text(s)) = rhs {
            let val = coerce_string_to_array(s, &ColType::Scalar(ScalarType::Text))?;
            value_to_rexpr(&val)
        } else {
            let (rarg, arg_ty) = resolve(scope, rhs, None, agg, params)?;
            match arg_ty {
                ResolvedType::Array(elem) if matches!(*elem, ResolvedType::Text) => {}
                ResolvedType::Null => {}
                _ => {
                    return Err(EngineError::new(
                        SqlState::UndefinedFunction,
                        "the #> / #>> path argument must be text[]",
                    ));
                }
            }
            rarg
        }
    } else {
        // `->` / `->>` take a key (text) or an array index (integer). A string literal stays text;
        // an integer literal stays integer; no adaptation is needed.
        let (rarg, arg_ty) = resolve(scope, rhs, None, agg, params)?;
        match arg_ty {
            ResolvedType::Text | ResolvedType::Int(_) | ResolvedType::Null => {}
            _ => {
                return Err(EngineError::new(
                    SqlState::UndefinedFunction,
                    format!(
                        "operator does not exist: jsonb {} {}",
                        json_op_symbol(op),
                        arg_ty.type_name()
                    ),
                ));
            }
        }
        rarg
    };
    Ok((
        RExpr::JsonGet {
            op: jop,
            base: Box::new(rbase),
            arg: Box::new(rarg),
        },
        result,
    ))
}

/// The node tree of a json/jsonb function argument: a `jsonb` value IS the canonical node; a `json`
/// value is parsed from its verbatim text on demand, preserving key order + duplicates (json.md §4).
pub(crate) fn json_arg_node(v: &Value) -> Result<JsonNode> {
    match v {
        Value::Jsonb(n) => Ok(n.clone()),
        Value::Json(s) => json::parse_preserving(s),
        _ => unreachable!("resolver restricts a json/jsonb function argument to json/jsonb"),
    }
}

/// Whether a parsed JSON node matches an `IS JSON [kind]` predicate's kind (json-sql-functions.md §5).
pub(crate) fn json_pred_kind_matches(node: &JsonNode, kind: JsonPredicateKind) -> bool {
    match kind {
        JsonPredicateKind::Value => true,
        JsonPredicateKind::Scalar => !matches!(node, JsonNode::Object(_) | JsonNode::Array(_)),
        JsonPredicateKind::Array => matches!(node, JsonNode::Array(_)),
        JsonPredicateKind::Object => matches!(node, JsonNode::Object(_)),
    }
}

/// The JSON image of any value — the `to_jsonb` kernel (json-sql-functions.md §2), also reused by
/// the json aggregates (B4). Numbers stay exact (`decimal`, never float); a `json`/`jsonb` value
/// canonicalizes; a 1-D array maps to a JSON array recursively (a NULL element → JSON null). The
/// type-info-dependent / float-divergent sources — composite (needs field names), float (the
/// binary→decimal divergence), datetime/uuid/bytea/interval (string-render divergences), and a
/// multidimensional array — are a deferred `0A000` follow-on.
pub(crate) fn value_to_node(v: &Value) -> Result<JsonNode> {
    Ok(match v {
        Value::Null => JsonNode::Null, // an array element (a top-level NULL is strict-propagated)
        Value::Bool(b) => JsonNode::Bool(*b),
        Value::Int(n) => JsonNode::Number(Decimal::from_i64(*n)),
        Value::Decimal(d) => JsonNode::Number(d.clone()),
        Value::Text(s) => JsonNode::String(s.clone()),
        Value::Jsonb(n) => n.clone(),
        Value::Json(s) => json::jsonb_in(s)?,
        Value::Array(arr) => {
            if arr.ndim() > 1 {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "to_jsonb of a multidimensional array is not supported yet",
                ));
            }
            let mut elems = Vec::with_capacity(arr.elements.len());
            for e in &arr.elements {
                elems.push(value_to_node(e)?);
            }
            JsonNode::Array(elems)
        }
        Value::Float32(_) | Value::Float64(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of a float value is not supported yet",
            ));
        }
        Value::Composite(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of a composite value is not supported yet",
            ));
        }
        Value::Uuid(_)
        | Value::Date(_)
        | Value::Timestamp(_)
        | Value::Timestamptz(_)
        | Value::Interval(_)
        | Value::Bytea(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of this type is not supported yet",
            ));
        }
        Value::Range(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of a range value is not supported yet",
            ));
        }
        Value::JsonPath(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "to_jsonb of a jsonpath value is not supported yet",
            ));
        }
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    })
}

/// One element's `json`-builder text image (json-sql-functions.md §2): a `json` value embeds VERBATIM,
/// a `jsonb` value its canonical (spaced) render, everything else the compact `to_jsonb` image. This
/// is how PG's `json_build_array`/`json_build_object` embed an argument's own json form.
pub(crate) fn elem_json_text(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Json(s) => s.clone(),
        Value::Jsonb(n) => json::jsonb_out(n),
        _ => json::json_compact_out(&value_to_node(v)?),
    })
}

/// The text form of a `json[b]_build_object` KEY argument (1-based `pos` for the error message). PG
/// coerces a key to text via the type's output: text as-is, integer/decimal/boolean rendered. A NULL
/// key is `22023`; a non-scalar key type is a deferred `0A000` follow-on.
pub(crate) fn object_key_text(v: &Value, pos: usize) -> Result<String> {
    Ok(match v {
        Value::Null => {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("argument {pos}: key must not be null"),
            ));
        }
        Value::Text(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::Decimal(d) => d.render(),
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        _ => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a json_build_object key of this type is not supported yet",
            ));
        }
    })
}

/// The `22004` raised when a `json_object` / `jsonb_object` key element is NULL.
pub(crate) fn object_key_null() -> EngineError {
    EngineError::new(
        SqlState::NullValueNotAllowed,
        "null value not allowed for object key",
    )
}

/// The display symbol for a jsonb accessor operator, for error messages.
pub(crate) fn json_op_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::JsonGet => "->",
        BinaryOp::JsonGetText => "->>",
        BinaryOp::JsonGetPath => "#>",
        BinaryOp::JsonGetPathText => "#>>",
        _ => "?",
    }
}

/// The "operator does not exist" error (42883) for a containment/positional operator whose operands
/// are neither arrays of a common element type nor ranges of a common element type (matches PG).
pub(crate) fn no_set_op_overload() -> EngineError {
    EngineError::new(
        SqlState::UndefinedFunction,
        "operator does not exist: the operands are not arrays or ranges of a common element type",
    )
}

/// Resolve a containment / overlap / positional operator (`@>` `<@` `&&` `<<` `>>` `&<` `&>` `-|-`),
/// choosing the axis by operand type: an array operand → the array containment surface
/// (array-functions.md §10, only `@>`/`<@`/`&&`); a range operand → the range boolean surface
/// (range-functions.md §3). The result is always boolean (strict — a NULL operand short-circuits to
/// NULL at eval). A non-array / non-range pair, or a positional operator on arrays, is 42883.
pub(crate) fn resolve_set_op(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // Pass 1: resolve both operands with no hint.
    let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
    let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
    // RANGE axis if either operand is a range. (The five positional operators are range-only; on a
    // non-range pair they fall through to the array branch below, which rejects them as 42883.)
    if matches!(lt, ResolvedType::Range(_)) || matches!(rt, ResolvedType::Range(_)) {
        return resolve_range_op(scope, op, lhs, rhs, rl, lt, rr, rt, agg, params);
    }

    // JSONB axis: only @>/<@ have a jsonb overload (json-sql-functions.md §1, J5). A jsonb operand
    // (or a string literal adapting to one) routes here; `&&`/the positional operators have no jsonb
    // overload and fall through to the array branch (42883). A json operand has no @> opclass (42883).
    if (matches!(op, BinaryOp::Contains | BinaryOp::ContainedBy))
        && (matches!(lt, ResolvedType::Jsonb) || matches!(rt, ResolvedType::Jsonb))
    {
        return resolve_jsonb_contains(scope, op, lhs, rhs, agg, params);
    }

    // ARRAY axis: only @>/<@/&& have an array overload (array-functions.md §10).
    let func = match op {
        BinaryOp::Contains => ArrayFunc::Contains,
        BinaryOp::ContainedBy => ArrayFunc::ContainedBy,
        BinaryOp::Overlaps => ArrayFunc::Overlaps,
        // A positional/adjacency operator on non-range operands — no array overload exists.
        _ => return Err(no_set_op_overload()),
    };
    let (mut rl, mut lt) = (rl, lt);
    let (mut rr, mut rt) = (rr, rt);
    // The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8), so a
    // bare `ARRAY[…]` constructor adapts to the column's element type (`xs @> ARRAY[20]`).
    let hint = match (&lt, &rt) {
        (ResolvedType::Array(e), _) => elem_scalar_hint(e),
        (_, ResolvedType::Array(e)) => elem_scalar_hint(e),
        _ => None,
    };
    // Pass 2: re-resolve the NON-NULL operands with the hint. A bare NULL (pass-1 type `Null`) is
    // left untyped — it defers in the anyarray slot and the boolean result is unaffected.
    if let Some(s) = hint {
        if !matches!(lt, ResolvedType::Null) {
            (rl, lt) = resolve(scope, lhs, Some(s), agg, params)?;
        }
        if !matches!(rt, ResolvedType::Null) {
            (rr, rt) = resolve(scope, rhs, Some(s), agg, params)?;
        }
    }

    // Both slots are `anyarray`: the element types must unify (a non-array / mismatch is 42883).
    let tys = [lt, rt];
    match_poly(&["anyarray", "anyarray"], &tys).ok_or_else(no_set_op_overload)?;
    Ok((
        RExpr::ArrayFunc {
            func,
            args: vec![rl, rr],
        },
        ResolvedType::Bool,
    ))
}

/// Resolve a jsonb containment operator `@>` / `<@` (json-sql-functions.md §1, J5). Both operands
/// must be `jsonb` (a bare string literal adapts via `jsonb_in`); a `json` operand has no @>
/// operator class (42883). `<@` resolves to `JsonContains` with the operands swapped (`a <@ b` is
/// `b @> a`). The result is boolean; the operator is strict (a NULL operand yields SQL NULL).
pub(crate) fn resolve_jsonb_contains(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // Resolve each operand with a jsonb context, so a bare `'{"a":1}'` string literal adapts.
    let resolve_jsonb = |e: &Expr, agg: &mut AggCtx, params: &mut ParamTypes| -> Result<RExpr> {
        let (r, t) = resolve(scope, e, Some(ScalarType::Jsonb), agg, params)?;
        match t {
            ResolvedType::Jsonb | ResolvedType::Null => Ok(r),
            _ => Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!(
                    "operator does not exist: {} {} {}",
                    t.type_name(),
                    binary_op_symbol(op),
                    "jsonb"
                ),
            )),
        }
    };
    let rl = resolve_jsonb(lhs, agg, params)?;
    let rr = resolve_jsonb(rhs, agg, params)?;
    // `a @> b` keeps the order; `a <@ b` is `b @> a`.
    let (a, b) = match op {
        BinaryOp::Contains => (rl, rr),
        BinaryOp::ContainedBy => (rr, rl),
        _ => unreachable!("resolve_jsonb_contains only handles @> / <@"),
    };
    Ok((
        RExpr::JsonContains {
            a: Box::new(a),
            b: Box::new(b),
        },
        ResolvedType::Bool,
    ))
}

/// Resolve a jsonb key-existence operator `?` / `?|` / `?&` (json-sql-functions.md §1, J5). The base
/// must be `jsonb` (a json base is 42883 — no operator). `?` takes a `text` key; `?|`/`?&` take a
/// `text[]` (a bare `'{a,b}'` string literal adapts). The result is boolean; the operator is strict.
pub(crate) fn resolve_json_has_key(
    scope: &Scope,
    kind: HasKeyKind,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let (rbase, base_ty) = resolve(scope, lhs, Some(ScalarType::Jsonb), agg, params)?;
    match base_ty {
        ResolvedType::Jsonb | ResolvedType::Null => {}
        _ => {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!(
                    "operator does not exist: {} {}",
                    base_ty.type_name(),
                    has_key_symbol(kind)
                ),
            ));
        }
    }
    let rarg = match kind {
        HasKeyKind::One => {
            // `?` takes a single text key.
            let (r, t) = resolve(scope, rhs, Some(ScalarType::Text), agg, params)?;
            match t {
                ResolvedType::Text | ResolvedType::Null => r,
                _ => {
                    return Err(EngineError::new(
                        SqlState::UndefinedFunction,
                        "the ? operator's right argument must be text",
                    ));
                }
            }
        }
        HasKeyKind::Any | HasKeyKind::All => {
            // `?|` / `?&` take a text[] (a bare string literal adapts via array_in).
            if let Expr::Literal(Literal::Text(s)) = rhs {
                let val = coerce_string_to_array(s, &ColType::Scalar(ScalarType::Text))?;
                value_to_rexpr(&val)
            } else {
                let (r, t) = resolve(scope, rhs, None, agg, params)?;
                match t {
                    ResolvedType::Array(elem) if matches!(*elem, ResolvedType::Text) => r,
                    ResolvedType::Null => r,
                    _ => {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            "the ?| / ?& operator's right argument must be text[]",
                        ));
                    }
                }
            }
        }
    };
    Ok((
        RExpr::JsonHasKey {
            kind,
            base: Box::new(rbase),
            arg: Box::new(rarg),
        },
        ResolvedType::Bool,
    ))
}

/// The display symbol for a key-existence operator, for error messages.
pub(crate) fn has_key_symbol(kind: HasKeyKind) -> &'static str {
    match kind {
        HasKeyKind::One => "?",
        HasKeyKind::Any => "?|",
        HasKeyKind::All => "?&",
    }
}

/// Resolve a jsonb `||` concatenation/merge (json-sql-functions.md §1, J6). Both operands must be
/// jsonb (a string literal adapts via `jsonb_in`). Result jsonb; strict.
pub(crate) fn resolve_jsonb_concat(
    scope: &Scope,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let resolve_jsonb = |e: &Expr, agg: &mut AggCtx, params: &mut ParamTypes| -> Result<RExpr> {
        let (r, t) = resolve(scope, e, Some(ScalarType::Jsonb), agg, params)?;
        match t {
            ResolvedType::Jsonb | ResolvedType::Null => Ok(r),
            _ => Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("operator does not exist: {} || jsonb", t.type_name()),
            )),
        }
    };
    let a = resolve_jsonb(lhs, agg, params)?;
    let b = resolve_jsonb(rhs, agg, params)?;
    Ok((
        RExpr::JsonConcat {
            a: Box::new(a),
            b: Box::new(b),
        },
        ResolvedType::Jsonb,
    ))
}

/// Resolve a jsonb delete operator: `-` (key `text` / index `int` / keys `text[]`) or `#-` (path
/// `text[]`) — json-sql-functions.md §1, J6. The base must be jsonb (a json base is 42883). The
/// form is chosen by the argument type; a bare `'{a,b}'` string literal adapts to `text[]`. Result
/// jsonb; strict.
pub(crate) fn resolve_jsonb_delete(
    scope: &Scope,
    is_path: bool,
    lhs: &Expr,
    rhs: &Expr,
    rbase: RExpr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let sym = if is_path { "#-" } else { "-" };
    let _ = lhs;
    // `#-` always takes a text[] path; `-` takes text / int / text[].
    let (kind, rarg) = if is_path {
        (
            DeleteKind::Path,
            resolve_text_array_arg(scope, rhs, sym, agg, params)?,
        )
    } else if let Expr::Literal(Literal::Text(_)) = rhs {
        // A bare string literal is a text key (`jsonb - 'a'`), NOT a text[].
        let (r, _) = resolve(scope, rhs, Some(ScalarType::Text), agg, params)?;
        (DeleteKind::Key, r)
    } else {
        let (r, t) = resolve(scope, rhs, None, agg, params)?;
        match t {
            ResolvedType::Text | ResolvedType::Null => (DeleteKind::Key, r),
            ResolvedType::Int(_) => (DeleteKind::Index, r),
            ResolvedType::Array(elem) if matches!(*elem, ResolvedType::Text) => {
                (DeleteKind::Keys, r)
            }
            _ => {
                return Err(EngineError::new(
                    SqlState::UndefinedFunction,
                    format!(
                        "operator does not exist: jsonb - {} (expected text, integer, or text[])",
                        t.type_name()
                    ),
                ));
            }
        }
    };
    Ok((
        RExpr::JsonDelete {
            kind,
            base: Box::new(rbase),
            arg: Box::new(rarg),
        },
        ResolvedType::Jsonb,
    ))
}

/// Resolve a `text[]` operator argument (the `#-` path / the `?|`/`?&` style): a bare string literal
/// `'{a,b}'` adapts via `array_in`; otherwise the resolved type must be `text[]` (or NULL). `sym` is
/// the operator symbol for the error message.
pub(crate) fn resolve_text_array_arg(
    scope: &Scope,
    rhs: &Expr,
    sym: &str,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    if let Expr::Literal(Literal::Text(s)) = rhs {
        let val = coerce_string_to_array(s, &ColType::Scalar(ScalarType::Text))?;
        return Ok(value_to_rexpr(&val));
    }
    let (r, t) = resolve(scope, rhs, None, agg, params)?;
    match t {
        ResolvedType::Array(elem) if matches!(*elem, ResolvedType::Text) => Ok(r),
        ResolvedType::Null => Ok(r),
        _ => Err(EngineError::new(
            SqlState::UndefinedFunction,
            format!("the {sym} operator's right argument must be text[]"),
        )),
    }
}

/// Resolve `jsonb_set` / `jsonb_insert` (json-sql-functions.md §2): `(target jsonb, path text[],
/// value jsonb [, flag boolean])` → jsonb. A bare `'{a,b}'` path literal adapts to text[] and a bare
/// string `value` literal adapts to jsonb. STRICT (the eval propagates any NULL). The optional flag
/// defaults to `true` for jsonb_set (create_if_missing) / `false` for jsonb_insert (insert_after).
pub(crate) fn resolve_jsonb_set_insert(
    scope: &Scope,
    name: &str,
    mode: json::PathSetMode,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.len() != 3 && args.len() != 4 {
        return Err(no_func_overload(name));
    }
    let (target, t0) = resolve(scope, &args[0], Some(ScalarType::Jsonb), agg, params)?;
    if !matches!(t0, ResolvedType::Jsonb | ResolvedType::Null) {
        return Err(no_func_overload(name));
    }
    let path = resolve_text_array_arg(scope, &args[1], name, agg, params)?;
    let (value, t2) = resolve(scope, &args[2], Some(ScalarType::Jsonb), agg, params)?;
    if !matches!(t2, ResolvedType::Jsonb | ResolvedType::Null) {
        return Err(no_func_overload(name));
    }
    let flag = if args.len() == 4 {
        let (f, tf) = resolve(scope, &args[3], Some(ScalarType::Bool), agg, params)?;
        if !matches!(tf, ResolvedType::Bool | ResolvedType::Null) {
            return Err(no_func_overload(name));
        }
        f
    } else {
        // Default: jsonb_set create_if_missing = true; jsonb_insert insert_after = false.
        value_to_rexpr(&Value::Bool(mode == json::PathSetMode::Set))
    };
    Ok((
        RExpr::JsonSetInsert {
            mode,
            args: vec![target, path, value, flag],
        },
        ResolvedType::Jsonb,
    ))
}

/// Resolve `json_object` / `jsonb_object` (json-sql-functions.md §2): one `text[]` of alternating
/// keys/values, or two `text[]` (keys, values). A bare `'{…}'` literal adapts to text[]. STRICT (the
/// eval propagates a NULL whole-array argument).
pub(crate) fn resolve_json_object(
    scope: &Scope,
    name: &str,
    json: bool,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.is_empty() || args.len() > 2 {
        return Err(no_func_overload(name));
    }
    let mut rargs = Vec::with_capacity(args.len());
    for a in args {
        rargs.push(resolve_text_array_arg(scope, a, name, agg, params)?);
    }
    let result = if json {
        ResolvedType::Json
    } else {
        ResolvedType::Jsonb
    };
    Ok((RExpr::JsonObjectFromArrays { json, args: rargs }, result))
}

/// Resolve a scalar jsonpath query function (P2, jsonpath.md §5): `(ctx jsonb, path jsonpath)`. A
/// bare string literal adapts (the context to jsonb, the path to a compiled jsonpath). STRICT.
pub(crate) fn resolve_jsonpath_fn(
    scope: &Scope,
    name: &str,
    kind: JsonPathFnKind,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let (ctx, path) = resolve_jsonpath_args(scope, name, args, agg, params)?;
    let result = match kind {
        JsonPathFnKind::Exists | JsonPathFnKind::Match => ResolvedType::Bool,
        JsonPathFnKind::QueryFirst | JsonPathFnKind::QueryArray => ResolvedType::Jsonb,
    };
    Ok((
        RExpr::JsonPathFn {
            kind,
            args: vec![ctx, path],
        },
        result,
    ))
}

/// Resolve the `(context jsonb, path jsonpath)` argument pair shared by the jsonpath query functions
/// (the SRF and the scalar forms). A bare string literal adapts: the context to jsonb, the path to a
/// compiled `jsonpath`. Exactly two args this slice (the optional `vars` / `silent` are a follow-on).
pub(crate) fn resolve_jsonpath_args(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, RExpr)> {
    if args.len() != 2 {
        return Err(no_func_overload(name));
    }
    let (ctx, ct) = resolve(scope, &args[0], Some(ScalarType::Jsonb), agg, params)?;
    if !matches!(ct, ResolvedType::Jsonb | ResolvedType::Null) {
        return Err(no_func_overload(name));
    }
    let (path, pt) = resolve(scope, &args[1], Some(ScalarType::JsonPath), agg, params)?;
    if !matches!(pt, ResolvedType::JsonPath | ResolvedType::Null) {
        return Err(no_func_overload(name));
    }
    Ok((ctx, path))
}

/// Recompile a `jsonpath` value's canonical text and evaluate it over a `jsonb` context value (the
/// shared kernel of the jsonpath query functions). A NULL context or path yields `None` (→ SQL NULL).
pub(crate) fn eval_jsonpath(ctx: &Value, path: &Value) -> Result<Option<Vec<JsonNode>>> {
    let node = match ctx {
        Value::Null => return Ok(None),
        _ => json_arg_node(ctx)?,
    };
    let text = match path {
        Value::Null => return Ok(None),
        Value::JsonPath(s) => s,
        _ => unreachable!("resolver restricts a jsonpath argument to jsonpath"),
    };
    let compiled = crate::jsonpath::JsonPath::compile(text)?;
    Ok(Some(crate::jsonpath::eval(&compiled, &node)?))
}

/// Extract a `text[]` value into `Vec<Option<String>>`, preserving NULL elements — `None` if the
/// value is not an array. Used by `json_object` (a NULL value → JSON null; a NULL key → 22004).
pub(crate) fn value_to_opt_text_array(v: &Value) -> Option<Vec<Option<String>>> {
    match v {
        Value::Array(arr) => Some(
            arr.elements
                .iter()
                .map(|e| match e {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Extract a `text[]` value into a path of strings — `None` if it is not an array or has a NULL
/// element (which propagates a SQL NULL through `jsonb_set`/`jsonb_insert`, like the `#-` path).
pub(crate) fn value_to_text_path(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(arr) => {
            let mut path = Vec::with_capacity(arr.elements.len());
            for e in &arr.elements {
                match e {
                    Value::Text(s) => path.push(s.clone()),
                    _ => return None,
                }
            }
            Some(path)
        }
        _ => None,
    }
}

/// Map a containment/positional `BinaryOp` to its range-against-range kernel (`RangeOp`).
pub(crate) fn range_op_for(op: BinaryOp) -> RangeOp {
    match op {
        BinaryOp::Contains => RangeOp::Contains,
        BinaryOp::ContainedBy => RangeOp::ContainedBy,
        BinaryOp::Overlaps => RangeOp::Overlaps,
        BinaryOp::StrictlyLeft => RangeOp::Before,
        BinaryOp::StrictlyRight => RangeOp::After,
        BinaryOp::NotExtendRight => RangeOp::Overleft,
        BinaryOp::NotExtendLeft => RangeOp::Overright,
        BinaryOp::Adjacent => RangeOp::Adjacent,
        _ => unreachable!("range_op_for is only called for the eight set/positional operators"),
    }
}

/// Resolve the RANGE axis of a containment/positional operator (range-functions.md §3), with both
/// operands already resolved (pass 1). The overload is chosen by the operand types: range×range (the
/// elements must match, else 42883) for every operator; the bare element overloads `range @> element`
/// and `element <@ range` re-resolve the element operand with the range's element type as the hint and
/// type-check assignability. A bare untyped `NULL` on one side is treated as a NULL range (the
/// range×range overload; eval yields NULL). Anything else is 42883.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_range_op(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    rl: RExpr,
    lt: ResolvedType,
    rr: RExpr,
    rt: ResolvedType,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    match (&lt, &rt) {
        // range × range (or a bare NULL on one side, taken as a NULL range): the elements must match.
        (ResolvedType::Range(le), ResolvedType::Range(re)) => {
            if resolved_range_element_scalar(le) != resolved_range_element_scalar(re) {
                return Err(no_set_op_overload());
            }
            let elem = resolved_range_element_scalar(le).expect("a range element is scalar");
            Ok((
                RExpr::RangeOp {
                    op: range_op_for(op),
                    args: vec![rl, rr],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        (ResolvedType::Range(le), ResolvedType::Null) => {
            let elem = resolved_range_element_scalar(le).expect("a range element is scalar");
            Ok((
                RExpr::RangeOp {
                    op: range_op_for(op),
                    args: vec![rl, rr],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        (ResolvedType::Null, ResolvedType::Range(re)) => {
            let elem = resolved_range_element_scalar(re).expect("a range element is scalar");
            Ok((
                RExpr::RangeOp {
                    op: range_op_for(op),
                    args: vec![rl, rr],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        // `range @> element` — the element overload of `@>` (the only operator with one). Re-resolve
        // the right operand with the range's element as the hint, then check it is assignable.
        (ResolvedType::Range(le), _) if op == BinaryOp::Contains => {
            let elem = resolved_range_element_scalar(le).expect("a range element is scalar");
            let (re_node, re_ty) = resolve(scope, rhs, Some(elem), agg, params)?;
            if !range_bound_assignable(&re_ty, elem) {
                return Err(no_set_op_overload());
            }
            Ok((
                RExpr::RangeOp {
                    op: RangeOp::ContainsElem,
                    args: vec![rl, re_node],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        // `element <@ range` — the element overload of `<@`.
        (_, ResolvedType::Range(re)) if op == BinaryOp::ContainedBy => {
            let elem = resolved_range_element_scalar(re).expect("a range element is scalar");
            let (le_node, le_ty) = resolve(scope, lhs, Some(elem), agg, params)?;
            if !range_bound_assignable(&le_ty, elem) {
                return Err(no_set_op_overload());
            }
            Ok((
                RExpr::RangeOp {
                    op: RangeOp::ElemContainedBy,
                    args: vec![le_node, rr],
                    elem,
                },
                ResolvedType::Bool,
            ))
        }
        _ => Err(no_set_op_overload()),
    }
}

/// Resolve a range SET operator (`+` union, `-` difference, `*` intersection — range-functions.md §4),
/// reached from [`resolve_binary`] when a `+`/`-`/`*` has a range operand (the operands are already
/// resolved). Both must be ranges over the SAME element type — a range × non-range, or a cross-element
/// pair, is `42883` (PG's "operator does not exist"); a bare untyped `NULL` beside a range is taken as
/// a NULL range (the range×range overload; eval → NULL, strict). The result is a range over that
/// element type. `range_merge` does NOT come through here (it is a function call — see
/// [`resolve_range_func`]); it shares the [`RExpr::RangeSetOp`] node with `op = Merge`.
pub(crate) fn resolve_range_set_op(
    op: BinaryOp,
    rl: RExpr,
    lt: ResolvedType,
    rr: RExpr,
    rt: ResolvedType,
) -> Result<(RExpr, ResolvedType)> {
    let elem = match (&lt, &rt) {
        (ResolvedType::Range(le), ResolvedType::Range(re)) => {
            let le = resolved_range_element_scalar(le).expect("a range element is scalar");
            let re = resolved_range_element_scalar(re).expect("a range element is scalar");
            if le != re {
                return Err(no_set_op_overload());
            }
            le
        }
        (ResolvedType::Range(le), ResolvedType::Null) => {
            resolved_range_element_scalar(le).expect("a range element is scalar")
        }
        (ResolvedType::Null, ResolvedType::Range(re)) => {
            resolved_range_element_scalar(re).expect("a range element is scalar")
        }
        // A range paired with a non-range (or any other combination) — no such operator.
        _ => return Err(no_set_op_overload()),
    };
    let setop = match op {
        BinaryOp::Add => RangeSetOp::Union,
        BinaryOp::Sub => RangeSetOp::Difference,
        BinaryOp::Mul => RangeSetOp::Intersect,
        _ => unreachable!("resolve_range_set_op is only called for +, -, *"),
    };
    Ok((
        RExpr::RangeSetOp {
            op: setop,
            args: vec![rl, rr],
        },
        ResolvedType::Range(Box::new(resolved_type_of(elem))),
    ))
}

/// Resolve a quantified array comparison `x op ANY/SOME/ALL(arr)` (array-functions.md §11): the
/// array spelling of `IN`. `x` (`lhs`) and the array operand resolve with the SAME literal
/// adaptation the comparison operators use — a bare-literal `x` adapts to the array's element type,
/// a bare `ARRAY[…]` operand adapts its elements to `x`'s type. The right operand must be an array
/// (a non-array side is `42809`; a bare untyped `NULL` is `42P18`); `x` and the element type must
/// be comparable (else `42883`, PG's operator-not-found). The result is always `boolean`; the 3VL
/// fold over the flattened elements reuses the `eq3`/`lt3`/`gt3` kernels at eval (the `IN`-list
/// membership machinery, generalized to all five operators and both quantifiers).
pub(crate) fn resolve_quantified(
    scope: &Scope,
    op: BinaryOp,
    all: bool,
    lhs: &Expr,
    array: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // Pass 1: resolve both operands with no hint.
    let (mut rl, mut lt) = resolve(scope, lhs, None, agg, params)?;
    let (mut ra, mut at) = resolve(scope, array, None, agg, params)?;
    // If `x` is a CONCRETE scalar (not itself an adaptable bare literal) and the array operand is a
    // bare `ARRAY[…]` constructor, re-resolve the array with `x`'s type as the element hint so the
    // constructor adapts (`c = ANY(ARRAY[1,2])` over an i32 column → i32[]). Harmless for a
    // column / cast operand (it ignores the hint).
    if !is_adaptable_operand(lhs) {
        if let Some(s) = ctx_of(&lt) {
            (ra, at) = resolve(scope, array, Some(s), agg, params)?;
        }
    }
    // If the array resolved to `E[]` and `x` is an adaptable bare literal, adapt `x` to `E` (with a
    // range check) — exactly the operand pairing `=` uses (`5 = ANY(i32[]_col)` lands `x` on i32).
    if let ResolvedType::Array(e) = &at {
        if is_adaptable_operand(lhs) {
            if let Some(s) = elem_scalar_hint(e) {
                (rl, lt) = resolve(scope, lhs, Some(s), agg, params)?;
            }
        }
    }
    // The right operand must be an array.
    let elem = match &at {
        ResolvedType::Array(e) => (**e).clone(),
        // A bare untyped NULL leaves the array type undeterminable — jed's polymorphic posture
        // (§11; the `unnest(NULL)` / §5 #6 precedent), a documented degenerate divergence from PG.
        ResolvedType::Null => {
            return Err(EngineError::new(
                SqlState::IndeterminateDatatype,
                "could not determine the array element type of a NULL ANY/ALL operand",
            ));
        }
        _ => {
            return Err(EngineError::new(
                SqlState::WrongObjectType,
                "op ANY/ALL (array) requires array on right side",
            ));
        }
    };
    // `x` and the element type must be comparable; PG reports operator-not-found (42883) here, NOT
    // the bare 42804 a plain `int = text` raises — matching AF4's element-mismatch posture (§10.2).
    classify_comparable(&lt, &elem).map_err(|_| {
        EngineError::new(
            SqlState::UndefinedFunction,
            format!(
                "operator does not exist: {} {} {}",
                lt.type_name(),
                binary_op_symbol(op),
                elem.type_name()
            ),
        )
    })?;
    let cop = match op {
        BinaryOp::Eq => CmpOp::Eq,
        BinaryOp::Ne => CmpOp::Ne,
        BinaryOp::Lt => CmpOp::Lt,
        BinaryOp::Gt => CmpOp::Gt,
        BinaryOp::Le => CmpOp::Le,
        BinaryOp::Ge => CmpOp::Ge,
        _ => unreachable!("the parser only builds a Quantified node for a comparison operator"),
    };
    Ok((
        RExpr::Quantified {
            op: cop,
            all,
            lhs: Box::new(rl),
            array: Box::new(ra),
        },
        ResolvedType::Bool,
    ))
}

/// The infix symbol for a comparison/arithmetic `BinaryOp`, for an `operator does not exist`
/// message (only the comparison operators reach `resolve_quantified`).
pub(crate) fn binary_op_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::Le => "<=",
        BinaryOp::Ge => ">=",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::Concat => "||",
        BinaryOp::Contains => "@>",
        BinaryOp::ContainedBy => "<@",
        BinaryOp::Overlaps => "&&",
        BinaryOp::StrictlyLeft => "<<",
        BinaryOp::StrictlyRight => ">>",
        BinaryOp::NotExtendRight => "&<",
        BinaryOp::NotExtendLeft => "&>",
        BinaryOp::Adjacent => "-|-",
        BinaryOp::JsonGet => "->",
        BinaryOp::JsonGetText => "->>",
        BinaryOp::JsonGetPath => "#>",
        BinaryOp::JsonGetPathText => "#>>",
        BinaryOp::JsonHasKey => "?",
        BinaryOp::JsonHasAnyKey => "?|",
        BinaryOp::JsonHasAllKeys => "?&",
        BinaryOp::JsonDeletePath => "#-",
        BinaryOp::JsonPathExists => "@?",
        BinaryOp::JsonPathMatch => "@@",
    }
}

/// Resolve the `||` array concatenation operator (array-functions.md §8): overload resolution over
/// the three `concat` catalog rows — `(anyarray,anyarray)` [array_cat], `(anyarray,anyelement)`
/// [array_append], `(anyelement,anyarray)` [array_prepend] — tried IN CATALOG ORDER, first match
/// wins. It is the operator spelling of the AF1 builders and reuses their kernels.
///
/// Two passes like `resolve_array_func`, with one deliberate difference: a **bare untyped NULL**
/// operand is left un-adapted. `match_poly` defers a bare NULL in an `anyarray` slot, so cat-first
/// makes `arr || NULL` / `NULL || arr` resolve to array_cat (the NULL array = identity), matching
/// PostgreSQL; adapting the bare NULL to a typed element would wrongly steer it into array_append.
pub(crate) fn resolve_concat(
    scope: &Scope,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let no_overload = || {
        EngineError::new(
            SqlState::UndefinedFunction,
            "operator does not exist: the || operands are not an array and a compatible element/array",
        )
    };

    // Pass 1: resolve both operands with no hint.
    let (mut rl, mut lt) = resolve(scope, lhs, None, agg, params)?;
    let (mut rr, mut rt) = resolve(scope, rhs, None, agg, params)?;
    // JSONB axis: a jsonb operand routes `||` to jsonb concat/merge (json-sql-functions.md §1, J6).
    if matches!(lt, ResolvedType::Jsonb) || matches!(rt, ResolvedType::Jsonb) {
        return resolve_jsonb_concat(scope, lhs, rhs, agg, params);
    }
    // The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8).
    let hint = match (&lt, &rt) {
        (ResolvedType::Array(e), _) => elem_scalar_hint(e),
        (_, ResolvedType::Array(e)) => elem_scalar_hint(e),
        _ => None,
    };
    // Pass 2: re-resolve the NON-NULL operands with the hint so a bare literal element / untyped
    // `ARRAY[…]` adapts to the array's element type. A bare NULL (pass-1 type `Null`) is skipped —
    // it must stay untyped so the cat-first overload order matches PG (see the doc comment).
    if let Some(s) = hint {
        if !matches!(lt, ResolvedType::Null) {
            (rl, lt) = resolve(scope, lhs, Some(s), agg, params)?;
        }
        if !matches!(rt, ResolvedType::Null) {
            (rr, rt) = resolve(scope, rhs, Some(s), agg, params)?;
        }
    }

    // Try the three concat overloads in catalog order; the first whose slots unify wins.
    let tys = [lt, rt];
    let overload = OPERATORS
        .iter()
        .filter(|o| o.kind == "concat")
        .find_map(|o| match_poly(o.arg_families, &tys).map(|elem| (o, elem)));
    let (desc, elem) = overload.ok_or_else(no_overload)?;
    let result = poly_result_type(desc.result, &elem)?;
    // The matched overload's slot pattern selects the kernel; the operands stay in source order
    // (array_prepend's kernel already reads vals[0]=element, vals[1]=array).
    let func = match desc.arg_families {
        ["anyarray", "anyarray"] => ArrayFunc::ArrayCat,
        ["anyarray", "anyelement"] => ArrayFunc::ArrayAppend,
        ["anyelement", "anyarray"] => ArrayFunc::ArrayPrepend,
        _ => unreachable!("concat overload has an unexpected slot pattern"),
    };
    Ok((
        RExpr::ArrayFunc {
            func,
            args: vec![rl, rr],
        },
        result,
    ))
}

/// Resolve the two operands of a binary operator, giving each adaptable literal the other
/// operand's type as context: a bare *integer* literal adopts the sibling's integer type (so
/// `small + 1` types `1` as i16, and `small + 100000` traps 22003 at resolve), and a
/// *string* literal adapts to a bytea sibling (decoding the hex input — types.md §6/§13),
/// otherwise staying text. When the sibling offers no usable context, the literal defaults to
/// its own family and the caller's family check reports the mismatch. This does NOT enforce a
/// family — `resolve_int_pair`/arithmetic and `classify_comparable` (comparison) layer that on top.
/// Resolve a two-numeric scalar function (gcd/lcm) by reusing the arithmetic operand-pair
/// resolution (literal adaptation), then settling the result type. Both operands must be integer
/// or decimal (a float/other operand → 42883); the result is the promoted integer type when both
/// are integer, else `decimal` (an integer operand promotes, as PG does). The kernel reads the
/// result type to range-check an integer result (gcd's i64::MIN abs / lcm overflow).
pub(crate) fn resolve_int_or_decimal_pair(
    scope: &Scope,
    name: &str,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, RExpr, ScalarType)> {
    let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
    let numeric_ok = |t: &ResolvedType| {
        matches!(
            t,
            ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null
        )
    };
    if !numeric_ok(&lt) || !numeric_ok(&rt) {
        return Err(no_func_overload(name));
    }
    let result = if matches!(lt, ResolvedType::Decimal) || matches!(rt, ResolvedType::Decimal) {
        ScalarType::Decimal
    } else {
        promote(&lt, &rt)
    };
    Ok((rl, rr, result))
}

/// A non-NULL integer/decimal value as a `Decimal` (the integer→decimal promotion gcd/lcm/div use).
pub(crate) fn value_to_decimal(v: &Value) -> Decimal {
    match v {
        Value::Int(n) => Decimal::from_i64(*n),
        Value::Decimal(d) => d.clone(),
        _ => unreachable!("expected an integer or decimal value"),
    }
}

/// gcd of two i64 by the Euclidean algorithm, returning the NON-NEGATIVE result. `None` iff the
/// magnitude is `i64::MIN` (its abs overflows i64) — the caller maps that to 22003, like PG. The
/// `b == -1` guard avoids the `i64::MIN % -1` overflow (the remainder is always 0).
pub(crate) fn gcd_i64(mut a: i64, mut b: i64) -> Option<i64> {
    while b != 0 {
        let t = if b == -1 { 0 } else { a % b };
        a = b;
        b = t;
    }
    a.checked_abs()
}

/// gcd of two decimals by the Euclidean algorithm over `rem`, result NON-NEGATIVE at scale
/// max(sₐ, s_b) (PG numeric gcd). The values share a fixed scale through the chain, so it reduces
/// to an integer gcd on the coefficients and always terminates. The final pad to the target scale
/// is exact (the value's natural scale never exceeds it).
pub(crate) fn gcd_decimal(a: &Decimal, b: &Decimal) -> Result<Decimal> {
    let target = a.scale().max(b.scale());
    let (mut x, mut y) = (a.clone(), b.clone());
    while !y.is_zero() {
        let r = x.rem(&y)?;
        x = y;
        y = r;
    }
    Ok(x.abs().round_to_scale(target))
}

/// lcm of two i64, NON-NEGATIVE: |a/gcd · b|, with checked arithmetic. `None` on i64 overflow
/// (the product, or the final abs) — the caller maps that (or an out-of-result-type magnitude) to
/// 22003, like PG. lcm(_, 0) = 0 (no division by the gcd, which would be 0).
pub(crate) fn lcm_i64(a: i64, b: i64) -> Option<i64> {
    if a == 0 || b == 0 {
        return Some(0);
    }
    let g = gcd_i64(a, b)?; // ≥ 1 for nonzero operands
    let prod = (a / g).checked_mul(b)?;
    prod.checked_abs()
}

/// lcm of two decimals, NON-NEGATIVE at scale max(sₐ, s_b): |a/gcd · b| (the a/gcd division is
/// exact). lcm(_, 0) = 0. A magnitude over the decimal value cap traps 22003 via the mul.
pub(crate) fn lcm_decimal(a: &Decimal, b: &Decimal) -> Result<Decimal> {
    let target = a.scale().max(b.scale());
    if a.is_zero() || b.is_zero() {
        return Ok(Decimal::zero(target));
    }
    let g = gcd_decimal(a, b)?;
    let prod = a.div(&g)?.mul(b)?;
    Ok(prod.abs().round_to_scale(target))
}

/// The 2201G raised by width_bucket for a bad count / equal-or-nonfinite bounds.
pub(crate) fn width_bucket_err(detail: &str) -> EngineError {
    EngineError::new(SqlState::InvalidArgumentForWidthBucketFunction, detail)
}

/// The MINIMUM scale that represents `d` exactly — its display scale minus trailing fractional zeros
/// (decimal.md, the shared engine of min_scale/trim_scale). Reduces the scale one step at a time:
/// round_to_scale(t-1) equals the value iff the digit at scale t is zero (otherwise it rounds,
/// changing the value), so the loop stops at the first non-zero fractional digit. Zero → 0.
pub(crate) fn min_scale_of(d: &Decimal) -> u32 {
    if d.is_zero() {
        return 0;
    }
    let mut t = d.scale();
    while t > 0 && d.round_to_scale(t - 1).cmp_value(d) == std::cmp::Ordering::Equal {
        t -= 1;
    }
    t
}

/// width_bucket over numerics (spec/functions/catalog.toml): floor((operand−low)·count/(high−low))
/// + 1, with 0 below low / count+1 at-or-above high, and the reversed (low > high) range. The bucket
/// is an EXACT truncated decimal quotient (all-positive in range, so trunc == floor). Returns the raw
/// index (the caller range-checks it to int4). `count > 0` is checked by the caller.
pub(crate) fn width_bucket_numeric(
    op: &Decimal,
    low: &Decimal,
    high: &Decimal,
    count: i64,
) -> Result<i64> {
    use std::cmp::Ordering;
    let cmp_bounds = low.cmp_value(high);
    if cmp_bounds == Ordering::Equal {
        return Err(width_bucket_err("lower bound cannot equal upper bound"));
    }
    let count_dec = Decimal::from_i64(count);
    // floor((hi_num − lo_num)·count / (hi_den − lo_den)), all operands ≥ 0 in range (trunc == floor).
    let bucket =
        |hi_num: &Decimal, lo_num: &Decimal, hi_den: &Decimal, lo_den: &Decimal| -> Result<i64> {
            let num = hi_num.sub(lo_num)?.mul(&count_dec)?;
            let den = hi_den.sub(lo_den)?;
            let q = num.sub(&num.rem(&den)?)?.div(&den)?.round_to_scale(0);
            let b = q
                .to_i64_round()
                .ok_or_else(|| overflow(ScalarType::Int32))?;
            Ok(b.saturating_add(1))
        };
    if cmp_bounds == Ordering::Less {
        // ascending low < high
        if op.cmp_value(low) == Ordering::Less {
            Ok(0)
        } else if op.cmp_value(high) != Ordering::Less {
            Ok(count.saturating_add(1))
        } else {
            bucket(op, low, high, low)
        }
    } else {
        // descending low > high
        if op.cmp_value(low) == Ordering::Greater {
            Ok(0)
        } else if op.cmp_value(high) != Ordering::Greater {
            Ok(count.saturating_add(1))
        } else {
            bucket(low, op, low, high)
        }
    }
}

/// width_bucket over f64 (spec/functions/catalog.toml): the same index in binary64 (a single
/// correctly-rounded chain, so cross-core identical). A NaN operand/bound → 2201G; a non-finite
/// bound → 2201G (the operand may be ±Inf, handled by the comparisons). Returns the raw index.
pub(crate) fn width_bucket_float(op: f64, low: f64, high: f64, count: i64) -> Result<i64> {
    if op.is_nan() || low.is_nan() || high.is_nan() {
        return Err(width_bucket_err(
            "operand, lower bound, and upper bound cannot be NaN",
        ));
    }
    if !low.is_finite() || !high.is_finite() {
        return Err(width_bucket_err("lower and upper bounds must be finite"));
    }
    if low == high {
        return Err(width_bucket_err("lower bound cannot equal upper bound"));
    }
    let cf = count as f64;
    let idx = if low < high {
        if op < low {
            0
        } else if op >= high {
            count.saturating_add(1)
        } else {
            (((op - low) / (high - low) * cf).floor() as i64).saturating_add(1)
        }
    } else if op > low {
        0
    } else if op <= high {
        count.saturating_add(1)
    } else {
        (((low - op) / (low - high) * cf).floor() as i64).saturating_add(1)
    };
    Ok(idx)
}

pub(crate) fn resolve_operand_pair(
    scope: &Scope,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType, RExpr, ResolvedType)> {
    let lhs_lit = is_adaptable_operand(lhs);
    let rhs_lit = is_adaptable_operand(rhs);
    let (rl, lt, rr, rt) = if lhs_lit && rhs_lit {
        // Two bare adaptable operands: no column context. Default an integer literal (and a
        // bind parameter) to i64; a string literal stays text (no bytea context — types.md §6).
        let (rl, lt) = resolve(scope, lhs, Some(ScalarType::Int64), agg, params)?;
        let (rr, rt) = resolve(scope, rhs, Some(ScalarType::Int64), agg, params)?;
        (rl, lt, rr, rt)
    } else if lhs_lit {
        let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
        let (rl, lt) = resolve(scope, lhs, ctx_of(&rt), agg, params)?;
        (rl, lt, rr, rt)
    } else if rhs_lit {
        let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
        let (rr, rt) = resolve(scope, rhs, ctx_of(&lt), agg, params)?;
        (rl, lt, rr, rt)
    } else {
        let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
        let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
        (rl, lt, rr, rt)
    };
    Ok((rl, lt, rr, rt))
}

/// Whether `e` is an *adaptable operand* — one that takes its type from its sibling: an integer
/// or string literal, or a bind parameter `$N` (spec/design/api.md §5). NULL, boolean, and
/// decimal literals do not take a sibling's context here.
pub(crate) fn is_adaptable_operand(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Literal(Literal::Int(_))
            | Expr::Literal(Literal::Decimal(_))
            | Expr::Literal(Literal::Text(_))
            | Expr::Param(_)
    )
}

/// The context type a sibling operand offers an adaptable operand. For an integer literal this
/// is the integer width it adopts; for a string literal, `bytea`/`uuid`/`text` (so it can decode
/// the hex/uuid input); a bind parameter additionally adopts a `decimal`/`boolean` sibling (a
/// literal ignores those — its arm keeps i64/text — so widening the mapping is safe). Only a
/// bare NULL offers no context.
pub(crate) fn ctx_of(ty: &ResolvedType) -> Option<ScalarType> {
    match ty {
        ResolvedType::Int(t) => Some(*t),
        ResolvedType::Bytea => Some(ScalarType::Bytea),
        ResolvedType::Uuid => Some(ScalarType::Uuid),
        ResolvedType::Text => Some(ScalarType::Text),
        ResolvedType::Bool => Some(ScalarType::Bool),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        // A json/jsonb sibling offers its type so a string literal parses as that type.
        ResolvedType::Json => Some(ScalarType::Json),
        ResolvedType::JsonPath => Some(ScalarType::JsonPath),
        ResolvedType::Jsonb => Some(ScalarType::Jsonb),
        ResolvedType::Null => None,
        // A composite/array/range sibling offers no scalar adaptation context.
        ResolvedType::Composite(_) | ResolvedType::Array(_) | ResolvedType::Range(_) => None,
        // A datetime sibling offers its type so a string literal parses as that datetime.
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
        // A date sibling offers its type so a string literal parses as a date.
        ResolvedType::Date => Some(ScalarType::Date),
        // An interval sibling offers its type so a string literal parses as an interval.
        ResolvedType::Interval => Some(ScalarType::Interval),
        // A float sibling offers its width so an integer/decimal literal ADAPTS to a float
        // context (decimal/int → float at the sibling's width — spec/design/float.md §4). A bare
        // string literal does NOT adapt to a float sibling (its Literal::Text arm keeps it text),
        // so widening the mapping is safe.
        ResolvedType::Float(st) => Some(*st),
    }
}

/// Require that an arithmetic operand is numeric (integer or decimal, or NULL); a boolean,
/// text, or bytea operand is a 42804 type error.
/// The result type of a temporal `+`/`-` (spec/design/interval.md §5), or `None` when neither
/// operand is temporal (interval / timestamp / timestamptz) — then arithmetic falls through to
/// the numeric path. `Some(Err)` is a temporal operand in an unsupported combination (42804). A
/// NULL operand adopts the other side's temporal type (so `timestamp ± NULL` types as timestamp
/// and evaluates to NULL).
pub(crate) fn temporal_arith_result(
    op: BinaryOp,
    lt: &ResolvedType,
    rt: &ResolvedType,
) -> Option<Result<ScalarType>> {
    use ResolvedType as R;
    let temporal = |t: &R| matches!(t, R::Interval | R::Timestamp | R::Timestamptz);
    if !temporal(lt) && !temporal(rt) {
        return None;
    }
    let l = if matches!(lt, R::Null) { rt } else { lt };
    let r = if matches!(rt, R::Null) { lt } else { rt };
    use BinaryOp::{Add, Sub};
    let st = match (op, l, r) {
        (Add | Sub, R::Interval, R::Interval) => ScalarType::Interval,
        (Add, R::Timestamp, R::Interval)
        | (Add, R::Interval, R::Timestamp)
        | (Sub, R::Timestamp, R::Interval) => ScalarType::Timestamp,
        (Add, R::Timestamptz, R::Interval)
        | (Add, R::Interval, R::Timestamptz)
        | (Sub, R::Timestamptz, R::Interval) => ScalarType::Timestamptz,
        (Sub, R::Timestamp, R::Timestamp) | (Sub, R::Timestamptz, R::Timestamptz) => {
            ScalarType::Interval
        }
        _ => {
            return Some(Err(type_error(
                "unsupported operand types for temporal arithmetic",
            )));
        }
    };
    Some(Ok(st))
}

/// The result type of a `date` arithmetic operator (spec/design/date.md §6): `date ± integer →
/// date`, `integer + date → date` (Add commutes; an integer of any width — the family matches
/// i16/i32/i64), `date − date → i32` (the count of days between, PG's int4), and `date ± interval
/// → timestamp` (the date widens to midnight, then the timestamp ± interval calendar shift — PG:
/// `date + interval` is a `timestamp`, not a date). `interval + date` commutes (Add only); there
/// is no `integer − date` nor `interval − date`. Any other combination involving a date is a
/// 42804 (PG reports "operator does not exist"; jed uses the datatype-mismatch code its other
/// arithmetic type errors use). A bare untyped NULL partner is NOT adopted — `date ± NULL` is a
/// 42804 (PG rejects the ambiguous form too); a typed NULL keeps its family and resolves here.
pub(crate) fn date_arith_result(
    op: BinaryOp,
    lt: &ResolvedType,
    rt: &ResolvedType,
) -> Result<ScalarType> {
    use BinaryOp::{Add, Sub};
    use ResolvedType as R;
    let st = match (op, lt, rt) {
        (Add, R::Date, R::Int(_)) | (Add, R::Int(_), R::Date) | (Sub, R::Date, R::Int(_)) => {
            ScalarType::Date
        }
        (Sub, R::Date, R::Date) => ScalarType::Int32,
        (Add, R::Date, R::Interval) | (Add, R::Interval, R::Date) | (Sub, R::Date, R::Interval) => {
            ScalarType::Timestamp
        }
        _ => return Err(type_error("unsupported operand types for date arithmetic")),
    };
    Ok(st)
}

/// The result type of an interval `×÷` number (spec/design/interval.md §5): `interval * number`,
/// `number * interval` (commute), `interval / number` → interval. `None` when no interval is
/// involved (or the op is not `*`/`/`). A NULL operand counts as a numeric partner (propagates).
/// `number / interval` and `interval × interval` return `None` here and fall to the ±-only
/// temporal rule, which reports the 42804.
pub(crate) fn interval_scale_result(
    op: BinaryOp,
    lt: &ResolvedType,
    rt: &ResolvedType,
) -> Option<Result<ScalarType>> {
    use ResolvedType as R;
    let l_iv = matches!(lt, R::Interval);
    let r_iv = matches!(rt, R::Interval);
    if !l_iv && !r_iv {
        return None;
    }
    let numeric = |t: &R| matches!(t, R::Int(_) | R::Decimal | R::Null);
    match op {
        BinaryOp::Mul if (l_iv && numeric(rt)) || (r_iv && numeric(lt)) => {
            Some(Ok(ScalarType::Interval))
        }
        BinaryOp::Div if l_iv && numeric(rt) => Some(Ok(ScalarType::Interval)),
        _ => None,
    }
}

/// A numeric factor value as an exact fraction `(num, den)` (`den > 0`): an integer is `(n, 1)`;
/// a decimal is parsed from its canonical string (interval.rs). Used by the interval `×÷` cascade.
pub(crate) fn factor_to_fraction(v: &Value) -> Result<(i128, i128)> {
    match v {
        Value::Int(n) => Ok((*n as i128, 1)),
        Value::Decimal(d) => crate::interval::parse_factor_decimal(&d.render()),
        _ => unreachable!("resolver guarantees a numeric interval-scale factor"),
    }
}

pub(crate) fn require_numeric_operand(ty: &ResolvedType) -> Result<()> {
    match ty {
        ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null => Ok(()),
        // Float reaches here only as the NON-float side of a mixed pair (a pure float × float pair
        // is routed before this) — int/decimal × float is a 42804, the strict island (float.md §6).
        ResolvedType::Bool
        | ResolvedType::Text
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
            Err(type_error("arithmetic operators require numeric operands"))
        }
    }
}

/// Require that a comparison operand pair is comparable (spec/types/compare.toml): both
/// numeric (integer and/or decimal — the integer promotes to decimal), both text, both
/// boolean, or both bytea (NULL counts as any). A cross-family pair (numeric/text,
/// boolean/non-boolean, bytea/non-bytea, …) is a 42804 type error — comparison is overloaded
/// across these families but never compares across them.
pub(crate) fn classify_comparable(lt: &ResolvedType, rt: &ResolvedType) -> Result<()> {
    use ResolvedType::{
        Array, Bool, Bytea, Composite, Date, Decimal, Float, Int, Interval, Json, JsonPath, Jsonb,
        Null, Range, Text, Timestamp, Timestamptz, Uuid,
    };
    match (lt, rt) {
        // json is NOT comparable: PostgreSQL ships no btree/hash operator class for `json`, so jed
        // matches it (spec/design/json.md §5). ANY json comparison — even json × json, json × jsonb,
        // or json × a bare NULL — is `42883` (operator does not exist), distinct from the
        // cross-family `42804` other types use. Must precede the jsonb arms so json × jsonb is 42883.
        (Json, _) | (_, Json) => Err(EngineError::new(
            SqlState::UndefinedFunction,
            "operator does not exist: json is not comparable",
        )),
        // jsonpath is likewise NOT comparable (PG ships no opclass — jsonpath.md §1): every
        // comparison is `42883`.
        (JsonPath, _) | (_, JsonPath) => Err(EngineError::new(
            SqlState::UndefinedFunction,
            "operator does not exist: jsonpath is not comparable",
        )),
        // jsonb IS comparable — PostgreSQL's total btree order (spec/design/json.md §5) — but only
        // with another jsonb (or a bare NULL). jsonb vs any other family is `42804` (jed's
        // cross-family convention, like uuid/bytea/range; a documented divergence from PG's 42883).
        (Jsonb, Jsonb) | (Jsonb, Null) | (Null, Jsonb) => Ok(()),
        (Jsonb, _) | (_, Jsonb) => Err(type_error(
            "cannot compare a jsonb value with a value of a different type",
        )),
        // Range comparison is the PG `range_cmp` total order (spec/design/ranges.md §6). Two ranges
        // are comparable iff they are over the **same element type** — `i32range × i32range` only,
        // never `i32range × i64range` or `i32range × i32` (no implicit cross-element range
        // comparison this slice; stricter than the int↔bigint scalar case, so the element
        // `ResolvedType`s must be *equal*, not merely comparable). A bare NULL is always comparable.
        (Range(_), Null) | (Null, Range(_)) => Ok(()),
        (Range(a), Range(b)) if a == b => Ok(()),
        (Range(_), Range(_)) => Err(type_error(
            "cannot compare ranges of different element types",
        )),
        (Range(_), _) | (_, Range(_)) => Err(type_error(
            "cannot compare a range value with a value of a different type",
        )),
        // Array comparison is element-wise (spec/design/array.md §5): two arrays are comparable iff
        // their element types are comparable (recursively). A bare NULL is always comparable; an
        // array vs any non-array is 42804.
        (Array(_), Null) | (Null, Array(_)) => Ok(()),
        (Array(a), Array(b)) => classify_comparable(a, b),
        (Array(_), _) | (_, Array(_)) => Err(type_error(
            "cannot compare an array value with a value of a different type",
        )),
        // Composite comparison is element-wise row comparison (spec/design/composite.md §5): two
        // composites are comparable iff they have the SAME field count and each corresponding
        // field pair is itself comparable (recursively — a nested composite recurses here, an
        // anonymous `ROW(…)` compares against a same-shape named type). A bare NULL is always
        // comparable (the comparison is unknown). A composite vs any non-composite, or a row-size
        // mismatch, or an incomparable field pair, is 42804.
        (Composite(_), Null) | (Null, Composite(_)) => Ok(()),
        (Composite(a), Composite(b)) => {
            if a.fields.len() != b.fields.len() {
                return Err(type_error("cannot compare rows of different sizes"));
            }
            for ((_, fa), (_, fb)) in a.fields.iter().zip(b.fields.iter()) {
                classify_comparable(fa, fb)?;
            }
            Ok(())
        }
        (Composite(_), _) | (_, Composite(_)) => Err(type_error(
            "cannot compare a composite value with a value of a different type",
        )),
        // Float is a STRICT ISLAND (spec/design/float.md §3/§6): comparable only float × float
        // (either width — a mixed-width pair promotes to f64 first, compare.toml `max-rank`)
        // or with a bare NULL. Float vs ANY other family (int/decimal included) is 42804 — jed
        // requires an explicit cast, a documented divergence from PG which promotes to float8.
        (Float(_), Float(_)) => Ok(()),
        (Float(_), Null) | (Null, Float(_)) => Ok(()),
        (Float(_), _) | (_, Float(_)) => Err(type_error(
            "cannot compare a float value with a value of a different type",
        )),
        // interval compares only within its own family (or with a bare NULL), by the canonical
        // span (spec/design/interval.md §2). interval vs any other family is a 42804.
        (Interval, Interval) => Ok(()),
        (Interval, Null) | (Null, Interval) => Ok(()),
        (Interval, _) | (_, Interval) => Err(type_error(
            "cannot compare an interval value with a value of a different type",
        )),
        // timestamp / timestamptz compare only within their own family (or with a bare NULL).
        // A mixed timestamp × timestamptz pair — or a datetime vs any other family — would need
        // a zone, so it is a 42804 type error (spec/design/timestamp.md §5).
        (Timestamp, Timestamp) | (Timestamptz, Timestamptz) => Ok(()),
        (Timestamp, Null) | (Null, Timestamp) | (Timestamptz, Null) | (Null, Timestamptz) => Ok(()),
        (Timestamp, _) | (_, Timestamp) | (Timestamptz, _) | (_, Timestamptz) => Err(type_error(
            "cannot compare a timestamp value with a value of a different type",
        )),
        // date compares only within its own family (or with a bare NULL), by the i32 day count
        // (spec/design/date.md §4). date vs any other family — including timestamp, which would
        // need a cast (a documented divergence from PG) — is a 42804.
        (Date, Date) => Ok(()),
        (Date, Null) | (Null, Date) => Ok(()),
        (Date, _) | (_, Date) => Err(type_error(
            "cannot compare a date value with a value of a different type",
        )),
        // Boolean compares only with boolean (or NULL); boolean with a number/text/bytea is a mismatch.
        (Bool, Int(_))
        | (Int(_), Bool)
        | (Bool, Text)
        | (Text, Bool)
        | (Bool, Decimal)
        | (Decimal, Bool)
        | (Bool, Bytea)
        | (Bytea, Bool)
        | (Bool, Uuid)
        | (Uuid, Bool) => Err(type_error(
            "cannot compare a boolean value with a non-boolean value",
        )),
        (Int(_), Text) | (Text, Int(_)) | (Decimal, Text) | (Text, Decimal) => Err(type_error(
            "cannot compare a text value with a numeric value",
        )),
        // bytea compares only with bytea (or NULL); bytea with a number, text, or uuid is a mismatch.
        (Bytea, Int(_))
        | (Int(_), Bytea)
        | (Bytea, Decimal)
        | (Decimal, Bytea)
        | (Bytea, Text)
        | (Text, Bytea)
        | (Bytea, Uuid)
        | (Uuid, Bytea) => Err(type_error(
            "cannot compare a bytea value with a non-bytea value",
        )),
        // uuid compares only with uuid (or NULL); uuid with a number or text is a mismatch
        // (the uuid/bool and uuid/bytea pairs are caught above).
        (Uuid, Int(_))
        | (Int(_), Uuid)
        | (Uuid, Decimal)
        | (Decimal, Uuid)
        | (Uuid, Text)
        | (Text, Uuid) => Err(type_error(
            "cannot compare a uuid value with a non-uuid value",
        )),
        // Same-family pairs (numeric/numeric incl. int↔decimal, text/text, bool/bool,
        // bytea/bytea, uuid/uuid) and any pairing with a bare NULL literal are comparable.
        _ => Ok(()),
    }
}

/// The `ScalarType` of an integer-typed resolved expression, or `None` for a NULL
/// literal or a non-integer type (used to pick a sibling literal's context).
pub(crate) fn int_type(ty: &ResolvedType) -> Option<ScalarType> {
    match ty {
        ResolvedType::Int(t) => Some(*t),
        _ => None,
    }
}

/// Wrap a `f32`-typed operand in an implicit `CAST(... AS f64)` so a mixed-width float
/// pair (compare or arith) computes at one width (spec/design/float.md §2/§5). A f64 or
/// non-float operand is returned unchanged; the caller decides when widening is needed.
pub(crate) fn widen_float_to_f64(node: RExpr, ty: &ResolvedType) -> RExpr {
    if matches!(ty, ResolvedType::Float(ScalarType::Float32)) {
        RExpr::Cast {
            inner: Box::new(node),
            target: ScalarType::Float64,
            typmod: None,
            varchar_len: None,
        }
    } else {
        node
    }
}

/// Resolve a float arithmetic pair to `(lhs, rhs, result_width)` with mixed widths promoted to
/// f64 (spec/design/float.md §5). Returns `None` when the pair is NOT a pure float pair (one
/// side is a non-float, non-NULL family) — the caller then raises the strict-island 42804. A
/// `float × NULL` pair adopts the float side's width (the NULL propagates at eval).
pub(crate) fn promote_float_arith(
    rl: RExpr,
    lt: ResolvedType,
    rr: RExpr,
    rt: ResolvedType,
) -> Option<(RExpr, RExpr, ScalarType)> {
    use ResolvedType::{Float, Null};
    let width = match (&lt, &rt) {
        (Float(a), Float(b)) => {
            if a.rank() >= b.rank() {
                *a
            } else {
                *b
            }
        }
        (Float(a), Null) | (Null, Float(a)) => *a,
        _ => return None,
    };
    // Promote a f32 operand to the common width when the result is f64.
    let (rl, rr) = if width == ScalarType::Float64 {
        (widen_float_to_f64(rl, &lt), widen_float_to_f64(rr, &rt))
    } else {
        (rl, rr)
    };
    Some((rl, rr, width))
}

/// The promotion-tower result type of two arithmetic operands: the higher-ranked
/// integer type, or i64 when both are untyped NULLs.
pub(crate) fn promote(a: &ResolvedType, b: &ResolvedType) -> ScalarType {
    match (int_type(a), int_type(b)) {
        (Some(x), Some(y)) => {
            if x.rank() >= y.rank() {
                x
            } else {
                y
            }
        }
        (Some(x), None) => x,
        (None, Some(y)) => y,
        (None, None) => ScalarType::Int64,
    }
}

/// LIKE requires both operands be `text` (or a bare NULL literal, which is comparable with
/// anything and makes the result NULL at eval). A non-text operand is a 42804 type error
/// (spec/design/grammar.md §22).
pub(crate) fn require_text_or_null(ty: &ResolvedType) -> Result<()> {
    match ty {
        ResolvedType::Text | ResolvedType::Null => Ok(()),
        _ => Err(type_error("LIKE requires text operands")),
    }
}

/// Unify a CASE's result-arm types (the THEN results + the ELSE, or `Null` for an implicit
/// ELSE) into one common type (spec/design/grammar.md §23): NULL-typed arms are dropped (they
/// adapt); an all-NULL CASE is `text` (PostgreSQL). The non-NULL arms must share a family — all
/// numeric unify to `decimal` if any is decimal, else the widest integer (the promotion tower);
/// otherwise they must all be the same non-numeric family (text/boolean/bytea). A cross-family
/// mix (e.g. integer and text) is 42804.
/// Unify the element types of an `ARRAY[…]` constructor into one element type (spec/design/array.md
/// §1). All-NULL → text (the PG unknown rule). All integer → the widest via the promotion tower (no
/// runtime coercion — every integer is an i64 value). Otherwise every element must be the SAME
/// family — a cross-family mix (including int + decimal) is a documented `42804` narrowing this
/// slice (the representation-changing coercion is deferred with `numeric(p,s)[]`).
pub(crate) fn unify_array_element_types(types: &[ResolvedType]) -> Result<ResolvedType> {
    let non_null: Vec<&ResolvedType> = types.iter().filter(|t| **t != ResolvedType::Null).collect();
    let Some(&first) = non_null.first() else {
        return Ok(ResolvedType::Text);
    };
    if non_null.iter().all(|t| matches!(t, ResolvedType::Int(_))) {
        let mut acc = first.clone();
        for t in &non_null[1..] {
            acc = ResolvedType::Int(promote(&acc, t));
        }
        return Ok(acc);
    }
    for t in &non_null[1..] {
        if std::mem::discriminant(*t) != std::mem::discriminant(first) {
            return Err(type_error(
                "array elements must all be of the same type".to_string(),
            ));
        }
    }
    Ok(first.clone())
}

/// A `2202E` array-subscript error (spec/design/array.md §11).
pub(crate) fn array_subscript_err(detail: &str) -> EngineError {
    EngineError::new(SqlState::ArraySubscriptError, detail.to_string())
}

/// Stack the evaluated elements of a **nested** `ARRAY[…]` constructor into a value of one higher
/// dimension (spec/design/array.md §4). The resolver guarantees every item resolved to an array; a
/// NULL sub-array or a sub-array of differing shape is a `2202E` ("multidimensional arrays must
/// have array expressions with matching dimensions"). Stacking empty sub-arrays yields the empty
/// array (PG: `ARRAY['{}'::int[]]` → `{}`).
pub(crate) fn build_nested_array(subs: Vec<Value>) -> Result<Value> {
    const MISMATCH: &str =
        "multidimensional arrays must have array expressions with matching dimensions";
    let mut arrs = Vec::with_capacity(subs.len());
    for s in subs {
        match s {
            Value::Array(a) => arrs.push(a),
            Value::Null => return Err(array_subscript_err(MISMATCH)),
            other => unreachable!("nested array constructor over a non-array: {other:?}"),
        }
    }
    let dims0 = arrs[0].dims.clone();
    let lbounds0 = arrs[0].lbounds.clone();
    for a in &arrs[1..] {
        if a.dims != dims0 || a.lbounds != lbounds0 {
            return Err(array_subscript_err(MISMATCH));
        }
    }
    if dims0.is_empty() {
        return Ok(Value::Array(ArrayVal::empty())); // all sub-arrays empty → empty array
    }
    let mut dims = vec![arrs.len()];
    dims.extend(dims0);
    let mut lbounds = vec![1i32];
    lbounds.extend(lbounds0);
    let mut elements = Vec::new();
    for a in arrs {
        elements.extend(a.elements);
    }
    Ok(Value::Array(ArrayVal {
        dims,
        lbounds,
        elements,
    }))
}

/// An evaluated slice bound: omitted (defer to the array's own bound), a NULL bound, or an integer.
#[derive(Clone, Copy)]
pub(crate) enum Bound {
    Omitted,
    Null,
    Val(i64),
}
