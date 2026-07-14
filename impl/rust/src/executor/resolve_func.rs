//! Function-call overload resolution (mirrors part of impl/go resolve_func.go): resolve_func_call and
//! the per-family resolvers (scalar, variadic, and the overload lookup/arg-family machinery), plus the
//! ParamTypes/PrivReq helpers that fall in this range.

use super::*;

/// Resolve a function call: an aggregate (COUNT/SUM/MIN/MAX/AVG), a scalar function
/// (abs/round/…, spec/design/functions.md §9), the named/defaulted `make_interval` (§11), or
/// 42883 (undefined_function) for any other name. Aggregates and scalar functions share the call
/// syntax (grammar.md §17); they are distinguished here, at resolve. Named notation (`name =>
/// value`) is valid only for a function that declares parameter names (make_interval); on every
/// other function it is rejected 42883 (PG's "function ... has no parameter named X").
pub(crate) fn resolve_func_call(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    arg_names: Option<&[Option<String>]>,
    star: bool,
    distinct: bool,
    filter: Option<&Expr>,
    variadic: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let lname = name.to_ascii_lowercase();
    // DISTINCT is an aggregate-only modifier: `abs(DISTINCT x)` is 42809 (PG's wrong_object_type,
    // "DISTINCT specified, but <fn> is not an aggregate function" — aggregates.md §5). Checked
    // before the per-kind dispatch so it covers every non-aggregate path (scalar, array, …).
    if distinct && !is_aggregate_name(&lname) {
        return Err(EngineError::new(
            SqlState::WrongObjectType,
            format!("DISTINCT specified, but {lname} is not an aggregate function"),
        ));
    }
    // FILTER is likewise aggregate-only: `abs(x) FILTER (WHERE …)` is 42809 (PG's wrong_object_type,
    // "FILTER specified, but <fn> is not an aggregate function" — aggregates.md §11). Same placement
    // as DISTINCT, so it covers every non-aggregate path before the per-kind dispatch.
    if filter.is_some() && !is_aggregate_name(&lname) {
        return Err(EngineError::new(
            SqlState::WrongObjectType,
            format!("FILTER specified, but {lname} is not an aggregate function"),
        ));
    }
    // The VARIADIC keyword is only valid on a VARIADIC function (array-functions.md §12). It
    // cannot decorate make_interval / an aggregate / an ordinary scalar function (PG: "VARIADIC
    // argument must be an array" arises only on a variadic function; a non-variadic function with
    // VARIADIC is 42883 — no such overload). Caught here before the per-kind dispatch.
    if variadic && !is_variadic_func_name(&lname) {
        return Err(no_func_overload(&lname));
    }
    if is_variadic_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        return resolve_variadic_func(scope, &lname, args, star, variadic, agg, params);
    }
    // make_interval is the one named/defaulted function — it keeps its own resolver (§11).
    if lname == "make_interval" {
        return resolve_make_interval(scope, args, arg_names, star, agg, params);
    }
    // make_timestamp / make_timestamptz are its named (un-defaulted) siblings (§11); make_timestamptz
    // is overloaded on arity (a session-zone 6-arg form + an explicit-zone 7-arg form). Their own
    // resolver picks the overload and normalizes named notation.
    if lname == "make_timestamp" || lname == "make_timestamptz" || lname == "make_date" {
        return resolve_make_timestamp(scope, &lname, args, arg_names, star, agg, params);
    }
    // lower/upper are overloaded across TWO families: the range accessors (range → element,
    // range-functions.md §1) and the text casing functions (text → text, collation.md §16). Resolve
    // the single argument once and branch on its type, BEFORE the by-name kind dispatch (which would
    // force the range path for both). (functions.md §9)
    if lname == "lower" || lname == "upper" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_lower_upper(scope, &lname, args, agg, params);
    }
    // `timezone(zone, value)` is the desugar of `value AT TIME ZONE zone` (grammar.md §49,
    // timezones.md §6) and a callable function in its own right. It is overloaded on the value's
    // family (timestamptz → timestamp, timestamp → timestamptz), so it resolves before the generic
    // by-name dispatch (which has no such polymorphism). (functions.md §9)
    if lname == "timezone" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_timezone(scope, args, agg, params);
    }
    // `date_trunc(unit, value[, zone])` (timezones.md §9.1) — polymorphic on the value family (the
    // result type is the value type) + an optional 3rd zone arg only on a timestamptz, so it resolves
    // before the generic by-name dispatch (which has no such polymorphism).
    if lname == "date_trunc" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_date_trunc(scope, args, agg, params);
    }
    // GROUPING(c1, …, ck) — the grouping-sets membership function (spec/design/aggregates.md §12).
    // It is not an aggregate (no DISTINCT/FILTER — those already errored 42809 above) and only
    // resolves inside a grouped query, so it is intercepted before the by-name dispatch.
    if lname == "grouping" {
        reject_named(&lname, arg_names)?;
        return resolve_grouping(scope, args, star, agg, params);
    }
    // `jsonb_set` / `jsonb_insert` (json-sql-functions.md §2) take a jsonb target, a text[] path (a
    // bare `'{a,b}'` literal adapts, like `#>`), a jsonb new value, and an optional boolean flag.
    // Hand-resolved (like the accessor operators) — the text[] + adapting-literal + optional-flag
    // signature is outside the catalog family mold.
    if lname == "jsonb_set" || lname == "jsonb_insert" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let mode = if lname == "jsonb_set" {
            json::PathSetMode::Set
        } else {
            json::PathSetMode::Insert
        };
        return resolve_jsonb_set_insert(scope, &lname, mode, args, agg, params);
    }
    // `json_object` / `jsonb_object` (json-sql-functions.md §2) build an object from one text[] of
    // alternating keys/values, or two text[] (keys, values). Hand-resolved (the text[] arg + adapting
    // literal are outside the catalog family mold), like jsonb_set.
    if lname == "json_object" || lname == "jsonb_object" {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_json_object(scope, &lname, lname == "json_object", args, agg, params);
    }
    // The scalar jsonpath query functions (P2, jsonpath.md §5): `(ctx jsonb, path jsonpath)`. Hand-
    // resolved (the jsonpath arg + adapting-literal are outside the catalog family mold).
    if let Some(kind) = match lname.as_str() {
        "jsonb_path_exists" => Some(JsonPathFnKind::Exists),
        "jsonb_path_query_first" => Some(JsonPathFnKind::QueryFirst),
        "jsonb_path_query_array" => Some(JsonPathFnKind::QueryArray),
        "jsonb_path_match" => Some(JsonPathFnKind::Match),
        _ => None,
    } {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_jsonpath_fn(scope, &lname, kind, args, agg, params);
    }
    // Otherwise the registry (the catalog descriptor tables) decides whether the name is an
    // aggregate, a scalar function, or undefined — no hand-written name lists (extensibility.md §5).
    if is_aggregate_name(&lname) {
        reject_named(&lname, arg_names)?;
        return resolve_aggregate(scope, &lname, args, star, distinct, filter, agg, params);
    }
    // The polymorphic array functions (array-functions.md §2) are also kind="function", so they
    // must be intercepted BEFORE the generic scalar path — their `anyarray`/`anyelement` slots need
    // §2 unification, which `lookup_scalar_overload`'s exact-family match cannot do.
    if is_array_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_array_func(scope, &lname, args, agg, params);
    }
    if is_range_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_range_func(scope, &lname, args, agg, params);
    }
    // A range CONSTRUCTOR (range-functions.md §2): a call whose name is a range type name/alias.
    // Like the array/range functions it is kind="function", so it must be intercepted BEFORE the
    // generic scalar path (its concrete-range result + element coercion are not the family-matched
    // scalar mold).
    if is_range_ctor_name(&lname) {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_range_ctor(scope, &lname, args, agg, params);
    }
    // The regex scalar functions (regex.md §8) are kind="function" too, but return text / text[] via
    // a dedicated RegexFunc node — the scalar-result path cannot carry the array result — so they are
    // intercepted before it, like the array/range functions above.
    if matches!(
        lname.as_str(),
        "regexp_replace"
            | "regexp_match"
            | "regexp_like"
            | "regexp_count"
            | "regexp_substr"
            | "regexp_instr"
    ) {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_regex_func(scope, &lname, args, agg, params);
    }
    // `div(a, b)` — the truncated (toward zero) integer quotient of two numerics, at scale 0 (PG
    // div(numeric, numeric)). Resolver-routed because the catalog name "div" already belongs to the
    // `/` operator (verify.rb keys uniqueness on [name, arg_families], so a function row would clash
    // with the `/` decimal row). Accepts integer + decimal operands (integers promote to numeric, as
    // PG does); a float/other operand → 42883. Two-arg only; else fall through to 42883.
    if lname == "div" && args.len() == 2 {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let (rl, lt, rr, rt) = resolve_operand_pair(scope, &args[0], &args[1], agg, params)?;
        let numeric_ok = |t: &ResolvedType| {
            matches!(
                t,
                ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null
            )
        };
        if !numeric_ok(&lt) || !numeric_ok(&rt) {
            return Err(no_func_overload("div"));
        }
        return Ok((
            RExpr::ScalarFunc {
                func: ScalarFunc::Div,
                args: vec![rl, rr],
                result: ScalarType::Decimal,
            },
            resolved_type_of(ScalarType::Decimal),
        ));
    }
    // `gcd(a, b)` / `lcm(a, b)` — resolver-routed for the same integer-promotion the arithmetic
    // operators do (a function row's "promoted" result would take only the first operand's width).
    // EXACT/in-contract; integer → promoted integer, a decimal operand → numeric.
    if (lname == "gcd" || lname == "lcm") && args.len() == 2 {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let (rl, rr, result) =
            resolve_int_or_decimal_pair(scope, &lname, &args[0], &args[1], agg, params)?;
        let func = if lname == "gcd" {
            ScalarFunc::Gcd
        } else {
            ScalarFunc::Lcm
        };
        return Ok((
            RExpr::ScalarFunc {
                func,
                args: vec![rl, rr],
                result,
            },
            resolved_type_of(result),
        ));
    }
    // `width_bucket(op, low, high, count)` — resolver-routed so the three value operands reconcile
    // across the integer/decimal families PG's implicit casts span (all-integer or mixed
    // integer/decimal → numeric; all-float → float). count must be integer. result int4.
    if lname == "width_bucket" && args.len() == 4 {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let mut rargs = Vec::with_capacity(4);
        let mut tys = Vec::with_capacity(4);
        for a in args {
            let (r, t) = resolve(scope, a, None, agg, params)?;
            rargs.push(r);
            tys.push(t);
        }
        // count (4th) is integer (a NULL adapts and propagates).
        if !matches!(tys[3], ResolvedType::Int(_) | ResolvedType::Null) {
            return Err(no_func_overload("width_bucket"));
        }
        // The value trio is EITHER all float (+NULL) → the float kernel, OR all integer/decimal
        // (+NULL) → the numeric kernel; a float mixed with a decimal/integer is 42883.
        let any_float = tys[..3].iter().any(|t| matches!(t, ResolvedType::Float(_)));
        let ok = |t: &ResolvedType| {
            if any_float {
                matches!(t, ResolvedType::Float(_) | ResolvedType::Null)
            } else {
                matches!(
                    t,
                    ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null
                )
            }
        };
        if !tys[..3].iter().all(ok) {
            return Err(no_func_overload("width_bucket"));
        }
        return Ok((
            RExpr::ScalarFunc {
                func: ScalarFunc::WidthBucket,
                args: rargs,
                result: ScalarType::Int32,
            },
            resolved_type_of(ScalarType::Int32),
        ));
    }
    // `mod(a, b)` is the function spelling of the `%` (mod) operator (catalog name "mod") — route it
    // to the SAME arithmetic machinery so mod() and % are observably identical (promotion, the
    // integer/decimal/float kernels, 22012/22003). PG's mod() is integer/numeric only; jed
    // additionally accepts mod(float), the `%`-over-float extension (oracle_overrides.toml). Only the
    // two-arg form is mod(); any other arity falls through to 42883.
    if lname == "mod" && args.len() == 2 {
        reject_named(&lname, arg_names)?;
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        return resolve_binary(scope, BinaryOp::Mod, &args[0], &args[1], agg, params);
    }
    if is_scalar_func_name(&lname) {
        reject_named(&lname, arg_names)?;
        return resolve_scalar_func(scope, &lname, args, star, agg, params);
    }
    Err(EngineError::new(
        SqlState::UndefinedFunction,
        format!("function does not exist: {name}"),
    ))
}

/// Resolve `regexp_replace`/`regexp_match` (regex.md §8) and the Oracle-compat `regexp_like`/
/// `regexp_count`/`regexp_substr`/`regexp_instr` (regex.md §8b) → a [`RExpr::RegexFunc`] whose result
/// type lives in the surrounding [`ResolvedType`]. All are STRICT (NULL arg propagates). The text
/// slots (source, pattern, flags) require text-or-null; the numeric slots (start/N/endoption/subexpr)
/// require integer-or-null (a non-integer is 42883, jed's strict-typing stance). A constant pattern
/// is precompiled once here (the precompilation contract, regex.md §5) — but only when the
/// case-insensitive `i` flag is statically known (the flags arg absent or a constant).
pub(crate) fn resolve_regex_func(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // (func, flags arg index, the integer-typed argument positions) per name + arity. Source(0) and
    // pattern(1) are always text; everything else is an integer except the flags slot (regex.md §8b).
    let (func, flags_idx, int_positions): (RegexFunc, Option<usize>, &[usize]) =
        match (name, args.len()) {
            ("regexp_replace", 3) => (RegexFunc::Replace, None, &[]),
            ("regexp_replace", 4) => (RegexFunc::Replace, Some(3), &[]),
            ("regexp_match", 2) => (RegexFunc::Match, None, &[]),
            ("regexp_match", 3) => (RegexFunc::Match, Some(2), &[]),
            ("regexp_like", 2) => (RegexFunc::Like, None, &[]),
            ("regexp_like", 3) => (RegexFunc::Like, Some(2), &[]),
            ("regexp_count", 2) => (RegexFunc::Count, None, &[]),
            ("regexp_count", 3) => (RegexFunc::Count, None, &[2]),
            ("regexp_count", 4) => (RegexFunc::Count, Some(3), &[2]),
            ("regexp_substr", 2) => (RegexFunc::Substr, None, &[]),
            ("regexp_substr", 3) => (RegexFunc::Substr, None, &[2]),
            ("regexp_substr", 4) => (RegexFunc::Substr, None, &[2, 3]),
            ("regexp_substr", 5) => (RegexFunc::Substr, Some(4), &[2, 3]),
            ("regexp_substr", 6) => (RegexFunc::Substr, Some(4), &[2, 3, 5]),
            ("regexp_instr", 2) => (RegexFunc::Instr, None, &[]),
            ("regexp_instr", 3) => (RegexFunc::Instr, None, &[2]),
            ("regexp_instr", 4) => (RegexFunc::Instr, None, &[2, 3]),
            ("regexp_instr", 5) => (RegexFunc::Instr, None, &[2, 3, 4]),
            ("regexp_instr", 6) => (RegexFunc::Instr, Some(5), &[2, 3, 4]),
            ("regexp_instr", 7) => (RegexFunc::Instr, Some(5), &[2, 3, 4, 6]),
            _ => return Err(no_func_overload(name)),
        };
    let mut rargs = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        if int_positions.contains(&i) {
            let (r, t) = resolve(scope, a, Some(ScalarType::Int64), agg, params)?;
            require_int_or_null(&t, name)?;
            rargs.push(r);
        } else {
            let (r, t) = resolve(scope, a, Some(ScalarType::Text), agg, params)?;
            require_text_or_null(&t)?;
            rargs.push(r);
        }
    }
    // Precompile a constant pattern (rargs[1]) once, folding it for a statically-constant `i` flag.
    let insensitive = match flags_idx.map(|i| &rargs[i]) {
        Some(RExpr::ConstText(f)) => Some(f.contains('i')),
        None => Some(false),
        Some(_) => None, // non-constant flags: defer compilation (and the `i` decision) to eval.
    };
    let program = match (&rargs[1], insensitive) {
        (RExpr::ConstText(pat), Some(insensitive)) => {
            let pat = if insensitive {
                let prop = crate::collation::loaded_property();
                crate::collation::fold_lower_simple(pat, prop.as_deref())
            } else {
                pat.clone()
            };
            Some(crate::regex::compile(&pat)?)
        }
        _ => None,
    };
    // A precompiled program carries the one-shot `compile_charged` cost flag mutated on first eval, so
    // a reused plan would under-charge the 2nd+ execute — never cache such a plan.
    if program.is_some() {
        params.uncacheable = true;
    }
    let result = match func {
        RegexFunc::Replace | RegexFunc::Substr => ResolvedType::Text,
        RegexFunc::Match => ResolvedType::Array(Box::new(ResolvedType::Text)),
        RegexFunc::Like => ResolvedType::Bool,
        RegexFunc::Count | RegexFunc::Instr => ResolvedType::Int(ScalarType::Int32),
    };
    Ok((
        RExpr::RegexFunc {
            func,
            args: rargs,
            program,
            compile_charged: std::cell::Cell::new(false),
        },
        result,
    ))
}

/// A numeric regexp_* argument (start/N/endoption/subexpr, regex.md §8b) must be an integer type, or
/// a bare NULL literal (which short-circuits the whole call to NULL at eval). A non-integer operand
/// is 42883 — jed's strict-typing stance rather than PG's implicit text→int cast.
pub(crate) fn require_int_or_null(ty: &ResolvedType, name: &str) -> Result<()> {
    match ty {
        ResolvedType::Int(_) | ResolvedType::Null => Ok(()),
        _ => Err(no_func_overload(name)),
    }
}

/// Named notation is only valid for a function that declares parameter names. Reject it on any
/// other function — PG's "function ... has no parameter named X" (42883).
pub(crate) fn reject_named(name: &str, arg_names: Option<&[Option<String>]>) -> Result<()> {
    if let Some(names) = arg_names {
        if let Some(Some(pn)) = names.iter().find(|n| n.is_some()) {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("function {name} has no parameter named \"{pn}\""),
            ));
        }
    }
    Ok(())
}

/// The lone scalar-function catalog row of this `name` (e.g. make_interval). Reads the
/// named/default/family metadata for named-notation resolution (functions.md §11) from the
/// generated catalog table (CLAUDE.md §5) rather than re-hardcoding it.
pub(crate) fn scalar_func_desc(name: &str) -> Option<&'static OperatorDesc> {
    OPERATORS
        .iter()
        .find(|o| o.kind == "function" && o.name == name)
}

/// The scalar-function catalog row of this `name` with the given `arity` — for a named function
/// overloaded on arity (make_timestamptz: a 6-arg session-zone form + a 7-arg explicit-zone form),
/// so named-notation resolution reads the right slot list (functions.md §11).
pub(crate) fn scalar_func_desc_arity(name: &str, arity: usize) -> Option<&'static OperatorDesc> {
    OPERATORS
        .iter()
        .find(|o| o.kind == "function" && o.name == name && o.arity as usize == arity)
}

/// The type context offered to an untyped literal in a function-argument slot of `family`, so it
/// adapts (functions.md §11): an integer slot offers i64, a float slot offers f64 (so a
/// bare `0`/`1.5` becomes f64 for `secs`), a text slot offers text (so a bare `'UTC'` adapts to the
/// `make_timestamptz` `timezone` slot). Other families offer no hint (the literal keeps its default
/// family, and the slot type-check catches a mismatch).
pub(crate) fn family_hint(family: &str) -> Option<ScalarType> {
    match family {
        "integer" => Some(ScalarType::Int64),
        "float" => Some(ScalarType::Float64),
        "text" => Some(ScalarType::Text),
        _ => None,
    }
}

/// Materialize a catalog DEFAULT (an integer-literal string, verify.rb-checked) as an `Expr` so
/// an omitted trailing argument resolves through the normal literal path — adapting to its slot's
/// family (e.g. "0" → f64 for `secs`). functions.md §11.
pub(crate) fn default_expr(lit: &str) -> Expr {
    let n: i64 = lit
        .parse()
        .expect("catalog arg_defaults are integer literals (verify.rb)");
    Expr::Literal(Literal::Int(n))
}

/// Map a call's positional + named arguments onto a function's positional parameter slots,
/// filling omitted trailing slots from `desc.arg_defaults` (PostgreSQL named notation + DEFAULTs,
/// functions.md §11). Returns the positional `Expr` vector of length `desc.arity`. Errors: 42601 a
/// positional arg after a named one (also caught at parse) or a duplicated name; 42883 an unknown
/// parameter name, too many arguments, or a missing non-defaulted slot (no matching overload).
pub(crate) fn normalize_named_args(
    desc: &OperatorDesc,
    args: &[Expr],
    arg_names: Option<&[Option<String>]>,
) -> Result<Vec<Expr>> {
    let arity = desc.arity as usize;
    let mut slots: Vec<Option<Expr>> = vec![None; arity];
    let mut seen_named = false;
    for (i, a) in args.iter().enumerate() {
        match arg_names.and_then(|ns| ns[i].as_ref()) {
            None => {
                if seen_named {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        "positional argument cannot follow named argument",
                    ));
                }
                if i >= arity {
                    return Err(no_func_overload(desc.name)); // too many positional arguments
                }
                slots[i] = Some(a.clone());
            }
            Some(pn) => {
                seen_named = true;
                let idx = desc
                    .arg_names
                    .iter()
                    .position(|p| p.eq_ignore_ascii_case(pn))
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedFunction,
                            format!("function {} has no parameter named \"{pn}\"", desc.name),
                        )
                    })?;
                if slots[idx].is_some() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!("argument name \"{pn}\" used more than once"),
                    ));
                }
                slots[idx] = Some(a.clone());
            }
        }
    }
    let first_defaulted = arity - desc.arg_defaults.len();
    let mut out = Vec::with_capacity(arity);
    for (i, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(e) => out.push(e),
            None if i >= first_defaulted => {
                out.push(default_expr(desc.arg_defaults[i - first_defaulted]))
            }
            None => return Err(no_func_overload(desc.name)), // missing required argument
        }
    }
    Ok(out)
}

/// Resolve `make_interval(years, months, weeks, days, hours, mins, secs)` — the engine's first
/// named + defaulted function (functions.md §11). Normalize named/positional args + defaults onto
/// the seven slots, resolve each with its declared family as the type hint (so a bare numeric
/// literal adapts to the `f64` `secs` slot), and emit a `MakeInterval` node. The arguments
/// keep their families (no promotion); a wrong family in a slot is 42883.
pub(crate) fn resolve_make_interval(
    scope: &Scope,
    args: &[Expr],
    arg_names: Option<&[Option<String>]>,
    star: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if star {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "* is only valid as the argument of COUNT",
        ));
    }
    let desc = scalar_func_desc("make_interval").expect("make_interval is in the catalog");
    let positional = normalize_named_args(desc, args, arg_names)?;
    let mut rargs = Vec::with_capacity(positional.len());
    for (i, e) in positional.iter().enumerate() {
        let fam = desc.arg_families[i];
        let (r, t) = resolve(scope, e, family_hint(fam), agg, params)?;
        // Type-check the resolved arg against its declared family. A NULL adapts (NULL
        // propagates). A f32 `secs` is read at its own width and widened losslessly to f64
        // at eval (no Cast node — so the cost matches the f64 case and the Go/TS cores).
        let ok = matches!(t, ResolvedType::Null)
            || (fam == "integer" && matches!(t, ResolvedType::Int(_)))
            || (fam == "float" && matches!(t, ResolvedType::Float(_)));
        if !ok {
            return Err(no_func_overload("make_interval"));
        }
        rargs.push(r);
    }
    Ok((
        RExpr::ScalarFunc {
            func: ScalarFunc::MakeInterval,
            args: rargs,
            result: ScalarType::Interval,
        },
        ResolvedType::Interval,
    ))
}

/// Resolve `make_timestamp(year, month, mday, hour, min, sec)` /
/// `make_timestamptz(…[, timezone])` — the named (but un-defaulted) make_interval siblings
/// (functions.md §11). `make_timestamptz` is overloaded on arity: a 6-arg form (interpret in the
/// session zone) and a 7-arg form (an explicit `timezone` text). The right overload is chosen by
/// whether the call supplies a 7th positional argument or names the `timezone` parameter; the
/// chosen catalog row then drives named-notation normalization (unknown name / too many / missing
/// required → 42883, a positional-after-named or duplicate → 42601). Each slot resolves with its
/// declared family as the type hint (a bare numeric literal adapts to the `f64` `sec` slot, a bare
/// string to the `text` `timezone` slot); a wrong family in a slot is 42883.
pub(crate) fn resolve_make_timestamp(
    scope: &Scope,
    name: &str, // "make_timestamp" | "make_timestamptz" | "make_date" (already lowercased)
    args: &[Expr],
    arg_names: Option<&[Option<String>]>,
    star: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if star {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "* is only valid as the argument of COUNT",
        ));
    }
    let is_tz = name == "make_timestamptz";
    // Pick the overload: the 7-arg explicit-zone form is selected by a 7th positional argument or a
    // named `timezone`; otherwise the 6-arg form. make_timestamp has only the 6-arg form and
    // make_date only its 3-arg (year, month, day) form.
    let arity = if name == "make_date" {
        3
    } else if is_tz {
        let positional = args
            .iter()
            .enumerate()
            .filter(|(i, _)| arg_names.and_then(|ns| ns[*i].as_ref()).is_none())
            .count();
        let names_timezone = arg_names.is_some_and(|ns| {
            ns.iter()
                .flatten()
                .any(|n| n.eq_ignore_ascii_case("timezone"))
        });
        if positional > 6 || names_timezone {
            7
        } else {
            6
        }
    } else {
        6
    };
    let desc = scalar_func_desc_arity(name, arity).ok_or_else(|| no_func_overload(name))?;
    let positional = normalize_named_args(desc, args, arg_names)?;
    let mut rargs = Vec::with_capacity(positional.len());
    for (i, e) in positional.iter().enumerate() {
        let fam = desc.arg_families[i];
        let (r, t) = resolve(scope, e, family_hint(fam), agg, params)?;
        // Type-check the resolved arg against its declared family. A NULL adapts (propagates). A
        // f32 `sec` is read at its own width and widened losslessly to f64 at eval (no Cast node, so
        // the cost matches the f64 case and the Go/TS cores).
        let ok = matches!(t, ResolvedType::Null)
            || (fam == "integer" && matches!(t, ResolvedType::Int(_)))
            || (fam == "float" && matches!(t, ResolvedType::Float(_)))
            || (fam == "text" && matches!(t, ResolvedType::Text));
        if !ok {
            return Err(no_func_overload(name));
        }
        rargs.push(r);
    }
    let (func, result) = if name == "make_date" {
        (ScalarFunc::MakeDate, ScalarType::Date)
    } else if is_tz {
        (ScalarFunc::MakeTimestamptz, ScalarType::Timestamptz)
    } else {
        (ScalarFunc::MakeTimestamp, ScalarType::Timestamp)
    };
    Ok((
        RExpr::ScalarFunc {
            func,
            args: rargs,
            result,
        },
        resolved_type_of(result),
    ))
}

/// Convert `make_interval`'s `secs` (double precision) to a microsecond count: one correctly-
/// rounded multiply, rounded half-away-from-zero to i64 (the engine's one mode — interval.md /
/// float.md §6). A non-finite or out-of-i64-range product traps 22008 (interval out of range),
/// matching PG. The result stays in-contract (the multiply + round are deterministic).
pub(crate) fn f64_to_micros(secs: f64) -> Result<i64> {
    let p = (secs * 1_000_000.0_f64).round(); // round-half-away-from-zero (f64::round)
    // 2^63 = 9_223_372_036_854_775_808.0 is the first f64 strictly above i64::MAX.
    if !p.is_finite() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&p) {
        return Err(EngineError::new(
            SqlState::DatetimeFieldOverflow,
            "interval out of range",
        ));
    }
    Ok(p as i64)
}

/// Resolve a scalar-function call (abs/round) into a per-row `ScalarFunc` node. Unlike an
/// aggregate it is legal in any context, so its arguments resolve in the SAME `agg` context
/// (a nested aggregate is still collected in a projection and 42803 in WHERE). The overload is
/// picked by the argument families; no match is 42883. spec/design/functions.md §9.
pub(crate) fn resolve_scalar_func(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    star: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if star {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "* is only valid as the argument of COUNT",
        ));
    }
    let mut rargs = Vec::with_capacity(args.len());
    let mut tys = Vec::with_capacity(args.len());
    for a in args {
        let (r, t) = resolve(scope, a, None, agg, params)?;
        rargs.push(r);
        tys.push(t);
    }
    // Pick the overload by argument families, its result type by the catalog `result` code, and
    // its kernel id by name (extensibility.md §5) — replacing the old hand-written (name,
    // arg-types) result match + name→variant match. abs's "promoted" gives the operand's own type
    // (its boundary range-checks for integers; its width for floats, the only `promoted` float fn);
    // round's decimal/integer overloads return numeric, its float overloads f64; the remaining
    // float functions return f64; the uuid extractors/generators return their catalog scalar id.
    let desc = lookup_scalar_overload(name, &tys).ok_or_else(|| no_func_overload(name))?;
    let result = scalar_result_type(desc.result, &tys);
    let func = scalar_func_id(name);
    // Promote float arguments to f64 when the function computes at f64 (every float
    // overload except `abs(f32)`, which keeps its width). The eval then sees one width.
    let widen_args = !matches!(func, ScalarFunc::Abs);
    if widen_args && result == ScalarType::Float64 {
        rargs = rargs
            .into_iter()
            .zip(tys.iter())
            .map(|(r, t)| widen_float_to_f64(r, t))
            .collect();
    }
    Ok((
        RExpr::ScalarFunc {
            func,
            args: rargs,
            result,
        },
        resolved_type_of(result),
    ))
}

/// The 42804 raised when a `VARIADIC` operand is not an array (array-functions.md §12 / §7).
pub(crate) fn variadic_not_array() -> EngineError {
    EngineError::new(
        SqlState::DatatypeMismatch,
        "VARIADIC argument must be an array",
    )
}

/// Resolve a VARIADIC scalar-function call (num_nulls / num_nonnulls — array-functions.md §12).
/// The lone catalog row's last parameter is variadic; the call is EITHER a spread of trailing
/// arguments OR (with the `VARIADIC` keyword) a single array passed directly. Non-strict
/// (`null = "none"`): the resolved node carries no blanket NULL short-circuit. Builds an
/// `RExpr::Variadic` node; the result type is the catalog `result` (i32 here), independent of
/// the arguments.
pub(crate) fn resolve_variadic_func(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    star: bool,
    variadic: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if star {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "* is only valid as the argument of COUNT",
        ));
    }
    let desc = scalar_func_desc(name).expect("a variadic function is in the catalog");
    let k = desc.arity as usize; // declared parameter count (the last is variadic)
    let var_family = desc.arg_families[k - 1]; // the variadic element family (last slot)

    let mut rargs = Vec::with_capacity(args.len());
    if variadic {
        // VARIADIC-array form: exactly `k` args (the fixed params + the one array). The fixed
        // params match their concrete families; the last operand MUST be an array (else 42804).
        if args.len() != k {
            return Err(no_func_overload(name));
        }
        for (i, a) in args.iter().enumerate() {
            let (r, t) = resolve(scope, a, None, agg, params)?;
            if i + 1 == k {
                // the variadic (array) operand
                match &t {
                    ResolvedType::Array(elem) => {
                        // "any" accepts any element type; a concrete variadic family must match.
                        if var_family != "any" && !family_matches(var_family, elem) {
                            return Err(no_func_overload(name));
                        }
                    }
                    // A non-array operand (incl. a bare untyped NULL) is 42804 — PG's exact code.
                    _ => return Err(variadic_not_array()),
                }
            } else if !family_matches(desc.arg_families[i], &t) {
                return Err(no_func_overload(name));
            }
            rargs.push(r);
        }
    } else {
        // Spread form: at least `k` args (so a variadic function needs ≥1 variadic arg —
        // num_nulls() is 42883). The json builders are the exception: a ZERO-arg spread is valid
        // (json_build_array() → [], json_build_object() → {}), so their floor is the fixed-param
        // count (k-1 = 0). The fixed params match their concrete families; every argument from the
        // variadic slot onward matches the variadic element family ("any" ⇒ all).
        let min_args = if json_build_classify(name).is_some() {
            k - 1
        } else {
            k
        };
        if args.len() < min_args {
            return Err(no_func_overload(name));
        }
        for (i, a) in args.iter().enumerate() {
            let (r, t) = resolve(scope, a, None, agg, params)?;
            let slot = if i < k - 1 {
                desc.arg_families[i]
            } else {
                var_family
            };
            if !family_matches(slot, &t) {
                return Err(no_func_overload(name));
            }
            rargs.push(r);
        }
    }

    let result = scalar_result_type(desc.result, &[]);
    // The json/jsonb builders share the spread/array-form validation above but their own eval node
    // and a json/jsonb result; the count functions (num_nulls/num_nonnulls) keep RExpr::Variadic.
    if let Some((kind, json)) = json_build_classify(name) {
        return Ok((
            RExpr::JsonBuild {
                kind,
                json,
                args: rargs,
                array_form: variadic,
            },
            resolved_type_of(result),
        ));
    }
    Ok((
        RExpr::Variadic {
            func: variadic_func_id(name),
            args: rargs,
            array_form: variadic,
        },
        resolved_type_of(result),
    ))
}

/// Classify a VARIADIC json/jsonb builder name → (kind, is-json). `None` for the count functions.
pub(crate) fn json_build_classify(name: &str) -> Option<(JsonBuildKind, bool)> {
    match name {
        "jsonb_build_array" => Some((JsonBuildKind::Array, false)),
        "json_build_array" => Some((JsonBuildKind::Array, true)),
        "jsonb_build_object" => Some((JsonBuildKind::Object, false)),
        "json_build_object" => Some((JsonBuildKind::Object, true)),
        _ => None,
    }
}

/// The 42803 raised for a non-aggregated column outside an aggregate with no GROUP BY.
pub(crate) fn grouping_error_column(name: &str) -> EngineError {
    EngineError::new(
        SqlState::GroupingError,
        format!(
            "column {name} must appear in the GROUP BY clause or be used in an aggregate function"
        ),
    )
}

/// Resolve `SELECT` items against the FROM scope into evaluable projections (any result type
/// is allowed in the select list, including boolean — `SELECT a = b`), each paired with its
/// output column name (spec/design/grammar.md §8). `*` expands across ALL relations in FROM
/// order, each relation's columns in catalog order (§15).
pub(crate) fn resolve_projections(
    scope: &Scope,
    items: &SelectItems,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(Vec<RExpr>, Vec<String>, Vec<ResolvedType>)> {
    match items {
        SelectItems::All => {
            // `*` with nothing to expand — a FROM-less SELECT — is PostgreSQL's exact error
            // (grammar.md §34). Qualifier-only rels don't count: they are RETURNING's old/new
            // pseudo-relations, and that scope always also carries the real relation.
            if scope.rels.iter().all(|r| r.qualifier_only) {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "SELECT * with no tables specified is not valid",
                ));
            }
            let mut nodes = Vec::new();
            let mut names = Vec::new();
            let mut types = Vec::new();
            // USING/NATURAL merged columns come FIRST, in join order (PostgreSQL — grammar.md §15):
            // `SELECT * FROM a JOIN b USING(k)` is `k, <a's other cols>, <b's other cols>`. Each
            // merge emits its surviving-side column; its underlying copies are in `hidden` and so are
            // skipped by the per-relation loop below (which is otherwise the plain `*` expansion).
            for m in &scope.merges {
                let c = scope.column_at(m.index);
                nodes.push(RExpr::Column(m.index));
                names.push(c.name.clone());
                types.push(resolved_type_of_col(&c.ty, scope.catalog));
            }
            // The RETURNING `old`/`new` pseudo-relations are qualifier-only: `*` expands the
            // real relations' columns exactly as before (grammar.md §32).
            for rel in scope.rels.iter().filter(|r| !r.qualifier_only) {
                for (i, c) in rel.table.columns.iter().enumerate() {
                    let idx = rel.offset + i;
                    if scope.hidden.contains(&idx) {
                        continue;
                    }
                    nodes.push(RExpr::Column(idx));
                    names.push(c.name.clone());
                    types.push(resolved_type_of_col(&c.ty, scope.catalog));
                }
            }
            Ok((nodes, names, types))
        }
        SelectItems::Items(items) => {
            let mut nodes = Vec::new();
            let mut names = Vec::new();
            let mut types = Vec::new();
            for it in items {
                // `t.*` expands the FROM relation labeled `qualifier` into one output column per
                // column, in catalog order (grammar.md §15) — like bare `*` but for one named
                // relation and mixable with other items. Resolved against the LOCAL scope only
                // (like bare `*`); an unknown label is 42P01, exactly as a qualified column ref.
                if let Expr::QualifiedStar { qualifier } = &it.expr {
                    let want = qualifier.to_ascii_lowercase();
                    let rel = scope
                        .rels
                        .iter()
                        .find(|r| r.label == want)
                        .ok_or_else(|| missing_from_entry(qualifier))?;
                    for (i, c) in rel.table.columns.iter().enumerate() {
                        nodes.push(RExpr::Column(rel.offset + i));
                        names.push(c.name.clone());
                        types.push(resolved_type_of_col(&c.ty, scope.catalog));
                    }
                    continue;
                }
                // `(expr).*` expands a composite base into one output column per field, in
                // declaration order (spec/design/composite.md §S4). The base AST is re-resolved
                // per field (Expr is Clone, RExpr is not) — deterministic, since resolution is
                // pure. An explicit alias on `(c).*` is rejected by PG; we ignore it here (the
                // parser does not attach one to a star item in practice).
                if let Expr::FieldStar { base } = &it.expr {
                    let (_, base_ty) = resolve(scope, base, None, agg, params)?;
                    let fields = match base_ty {
                        ResolvedType::Composite(c) => c.fields,
                        other => {
                            return Err(EngineError::new(
                                SqlState::WrongObjectType,
                                format!(
                                    "column notation .* applied to type {}, which is not a composite type",
                                    other.type_name()
                                ),
                            ));
                        }
                    };
                    for (i, (fname, fty)) in fields.into_iter().enumerate() {
                        let (bn, _) = resolve(scope, base, None, agg, params)?;
                        nodes.push(RExpr::Field {
                            base: Box::new(bn),
                            index: i,
                        });
                        names.push(fname);
                        types.push(fty);
                    }
                    continue;
                }
                let (node, ty) = resolve(scope, &it.expr, None, agg, params)?;
                names.push(match &it.alias {
                    Some(a) => a.clone(),
                    None => output_name(scope, &it.expr),
                });
                nodes.push(node);
                types.push(ty);
            }
            Ok((nodes, names, types))
        }
    }
}

/// The output column name of an un-aliased select item (spec/design/grammar.md §8/§15): a
/// bare or qualified column reference takes the catalog's canonical name (the `CREATE TABLE`
/// spelling, not the SELECT spelling, and never the qualifier — so casing/qualifier never
/// leaks); every other expression takes the fixed `?column?`. The column is known to exist —
/// `resolve` validated it.
pub(crate) fn output_name(scope: &Scope, e: &Expr) -> String {
    match e {
        // A bare/qualified column takes the catalog's canonical name, whether it resolves to a
        // local relation or (correlated) an enclosing one — `column_of` handles both.
        Expr::Column(name) => match scope.resolve_bare(name) {
            Ok(r) => scope.column_of(r).name.clone(),
            Err(_) => name.clone(),
        },
        Expr::QualifiedColumn { qualifier, name } => match scope.resolve_qualified(qualifier, name)
        {
            Ok(r) => scope.column_of(r).name.clone(),
            Err(_) => name.clone(),
        },
        // An un-aliased aggregate call is named by its lowercased function name (PG;
        // spec/design/grammar.md §8). A field selection takes the FIELD name (PG names the
        // output column after the selected field). Any other expression takes `?column?`.
        Expr::FuncCall { name, .. } => name.to_ascii_lowercase(),
        // The fixed keyword lowercased (PG; grammar.md §51) — no expression printer needed.
        Expr::Coalesce(_) => "coalesce".to_string(),
        // The fixed keyword lowercased (PG; grammar.md §52).
        Expr::GreatestLeast { greatest, .. } => {
            if *greatest { "greatest" } else { "least" }.to_string()
        }
        Expr::FieldAccess { field, .. } => field.to_ascii_lowercase(),
        // A subscript takes the base array's name (PG names `a[1]` after `a`); a chained subscript
        // `a[1][2]` recurses to the same base name. A non-column base falls through to `?column?`.
        Expr::Subscript { base, .. } => output_name(scope, base),
        _ => "?column?".to_string(),
    }
}

/// Resolve a bare `ORDER BY` name against the SELECT output columns — PostgreSQL's SQL92 rule that
/// an `ORDER BY` simple name binds an **output** column (an `AS` alias or an item's derived name —
/// grammar.md §8/§10) BEFORE an input column, the opposite of `GROUP BY`'s precedence. Returns the
/// matching select-list item's **expression** (the caller routes it exactly like the same ordinal:
/// a plain column stays on the slot fast path, a computed item is materialized), or `None` when no
/// output name matches (the caller falls back to the FROM scope, the prior behavior). Matching is
/// case-insensitive (§8). Only an explicit list is scanned — with `*` the output names are the scope
/// columns, so the FROM-scope fallback already binds the same column. Two items of the same name
/// with DIFFERENT expressions are ambiguous (`42702`); the same expression twice is not
/// (`SELECT a, a … ORDER BY a`), matching PostgreSQL.
pub(crate) fn order_alias_match<'a>(
    items: &'a SelectItems,
    name: &str,
    scope: &Scope,
) -> Result<Option<&'a Expr>> {
    let SelectItems::Items(items) = items else {
        return Ok(None);
    };
    let mut found: Option<&Expr> = None;
    for it in items {
        let oname = match &it.alias {
            Some(a) => a.clone(),
            None => output_name(scope, &it.expr),
        };
        if !oname.eq_ignore_ascii_case(name) {
            continue;
        }
        match found {
            None => found = Some(&it.expr),
            Some(prev) if *prev != it.expr => {
                return Err(EngineError::new(
                    SqlState::AmbiguousColumn,
                    format!("ORDER BY \"{name}\" is ambiguous"),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(found)
}

/// Resolve a WHERE / ON expression: it must resolve to boolean (or an untyped NULL, which
/// is always unknown → no rows). An integer-valued WHERE/ON is a 42804 type error.
pub(crate) fn resolve_boolean_filter(
    scope: &Scope,
    e: &Expr,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    // WHERE / ON filters run before any grouping, so an aggregate here is 42803 (Forbidden).
    let mut agg = AggCtx::Forbidden;
    let (node, ty) = resolve(scope, e, None, &mut agg, params)?;
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(node),
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
        | ResolvedType::Range(_) => Err(type_error("argument of WHERE must be boolean")),
    }
}

/// Per-statement accumulator of bind-parameter types, inferred from context during resolve
/// (spec/design/api.md §5). `types[i]` is the inferred scalar type of `$(i+1)`; `None` marks a
/// parameter referenced before any context fixed its type. Shared across every clause of a
/// statement (so a `$1` used in both WHERE and the select list unifies), then `finalize`d.
#[derive(Default)]
pub(crate) struct ParamTypes {
    pub(crate) types: Vec<Option<ScalarType>>,
    /// Set during resolution when a node is created that makes the resolved plan un-reusable across
    /// executions: an `RExpr::Subquery` (the uncorrelated-subquery fold rewrites it to a constant
    /// baking in THIS execution's bound params) or a precompiled-regex node (whose one-shot
    /// `compile_charged` cost flag mutates during eval). A prepared statement's plan cache fills only
    /// when this stayed false — flagging at the node's birth is complete regardless of where in the
    /// plan tree it lands (spec/design/api.md §2.4).
    pub(crate) uncacheable: bool,
    /// Set during resolution when a node is created whose value depends on statement-execution
    /// context rather than its inputs alone: the runtime text→date cast (STABLE — its input
    /// grammar admits the clock-relative specials) and the `DateClock` clock-relative date
    /// literal (`'today'`/`'now'`/…, date.md §6). The expression-index gate consults it to reject
    /// such an expression 42P17 (indexes.md §2), the same way PostgreSQL's stable `date_in` is
    /// unindexable. Orthogonal to `uncacheable`: these nodes re-evaluate per execution, so the
    /// resolved plan stays cacheable.
    pub(crate) nonimmutable: bool,
}

/// Resolve a date-context string literal naming one of the special values beyond ±infinity
/// (date.md §6): `'epoch'` folds to the constant 1970-01-01 like any date literal, while the
/// CLOCK-RELATIVE words `'today'` / `'now'` / `'tomorrow'` / `'yesterday'` become the STABLE
/// `DateClock` node — the statement clock's day in the session zone, computed at EVAL and never
/// folded at resolve. (PostgreSQL folds the literal at parse — the frozen-'today'
/// DEFAULT/index/prepared-statement footgun — a documented divergence; jed's node re-evaluates
/// per execution, so a cached plan tracks the clock.) The node flags the plan non-immutable,
/// exactly like the runtime text→date cast (42P17 in an index expression). `None` for an
/// ordinary date string, which takes the caller's normal parse-to-constant path.
fn date_clock_literal(s: &str, params: &mut ParamTypes) -> Option<(RExpr, ResolvedType)> {
    let (offset_days, epoch) = crate::date::date_clock_special(s)?;
    if epoch {
        return Some((RExpr::ConstDate(0), ResolvedType::Date));
    }
    params.nonimmutable = true;
    Some((RExpr::DateClock { offset_days }, ResolvedType::Date))
}

impl ParamTypes {
    /// Record that `$(idx0+1)` appears with context type `ty` (`None` = no context here).
    /// Unifies with any prior inference for the same index: equal types agree, two integer
    /// widths widen to the wider, an incompatible concrete pair is 42804.
    pub(crate) fn note(&mut self, idx0: usize, ty: Option<ScalarType>) -> Result<()> {
        if idx0 >= self.types.len() {
            self.types.resize(idx0 + 1, None);
        }
        if let Some(new) = ty {
            self.types[idx0] = Some(match self.types[idx0] {
                None => new,
                Some(old) => unify_param_type(old, new, idx0)?,
            });
        }
        Ok(())
    }

    /// Finalize to the ordered parameter types. A slot referenced but never typed — including a
    /// gap in `$1..$N` — is 42P18 indeterminate_datatype.
    pub(crate) fn finalize(self) -> Result<Vec<ScalarType>> {
        let mut out = Vec::with_capacity(self.types.len());
        for (i, t) in self.types.into_iter().enumerate() {
            match t {
                Some(ty) => out.push(ty),
                None => {
                    return Err(EngineError::new(
                        SqlState::IndeterminateDatatype,
                        format!("could not determine data type of parameter ${}", i + 1),
                    ));
                }
            }
        }
        Ok(out)
    }
}

/// Unify two inferred types for the same bind parameter: equal agrees; two integer widths
/// widen to the wider (so `$1` works against both an i16 and an i32 column); any other
/// mismatch is 42804 (spec/design/api.md §5).
pub(crate) fn unify_param_type(a: ScalarType, b: ScalarType, idx0: usize) -> Result<ScalarType> {
    if a == b {
        return Ok(a);
    }
    if a.is_integer() && b.is_integer() {
        return Ok(if a.rank() >= b.rank() { a } else { b });
    }
    Err(EngineError::new(
        SqlState::DatatypeMismatch,
        format!("inconsistent types inferred for parameter ${}", idx0 + 1),
    ))
}

/// Coerce each supplied bind value to its inferred parameter type, two-phase / all-or-nothing
/// like INSERT (spec/design/api.md §5): a count mismatch is 42601 and every value is validated
/// up front (22003/42804/22P02/23502 via `store_value`) before any row is touched.
pub(crate) fn bind_params(supplied: &[Value], types: &[ScalarType]) -> Result<Vec<Value>> {
    if supplied.len() != types.len() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            format!(
                "bind parameter count mismatch: statement expects {}, got {}",
                types.len(),
                supplied.len()
            ),
        ));
    }
    let mut bound = Vec::with_capacity(types.len());
    for (i, (v, ty)) in supplied.iter().zip(types).enumerate() {
        // A bound parameter is coerced exactly like a literal in that position: typmod is
        // unconstrained (a comparison/insert against a column re-applies the column typmod),
        // not_null is false (NULL is a legal bound value; a NOT NULL target re-checks at store).
        bound.push(store_value(
            v.clone(),
            *ty,
            None,
            None,
            false,
            &format!("${}", i + 1),
        )?);
    }
    Ok(bound)
}

/// A DDL statement (CREATE/DROP TABLE) has no expressions and so takes no bind parameters;
/// supplying any is a 42601 (spec/design/api.md §5).
pub(crate) fn reject_params_for_ddl(params: &[Value]) -> Result<()> {
    if params.is_empty() {
        Ok(())
    } else {
        Err(EngineError::new(
            SqlState::SyntaxError,
            "bind parameters are not allowed in a DDL statement",
        ))
    }
}

// ================================================================================================
// EXPLAIN — render the planner's chosen plan as a deterministic, cross-core-identical result set
// (spec/design/explain.md). The output is an ordinary query Outcome with five columns:
//
//   depth  i32   the plan node's nesting level (0-based), from a pre-order DFS of the plan tree
//   node   text  the operator label (a fixed vocabulary — the §8 cross-core spelling contract)
//   detail text  the node's attributes (access path, keys, counts); "-" when it has none
//   est_rows i64 the deterministic estimated output rows for this subtree
//   est_cost i64 the deterministic cumulative estimated cost for this subtree
//
// Rows are emitted in pre-order, so the row order is deterministic by construction — the corpus
// asserts an EXPLAIN with `nosort`. Every cell is non-empty and free of leading/trailing whitespace
// (indentation is carried by `depth`, never whitespace), so an empty detail uses the "-" sentinel
// (spec/design/explain.md §2). Plain EXPLAIN renders the plan WITHOUT executing the inner statement;
// EXPLAIN ANALYZE runs it and reports the actual accrued cost + row count on an `Analyze` root (§3).
// This mirrors the Go/TS core renderers token-for-token — the shared corpus pins the byte output.
// ================================================================================================

/// Accumulates the rendered plan rows.
#[derive(Default)]
pub(crate) struct ExplainRender {
    pub(crate) rows: Vec<Vec<Value>>,
    estimates: Vec<(i64, i64)>,
    next_estimate: usize,
}

impl ExplainRender {
    pub(crate) fn with_estimates(estimates: Vec<(i64, i64)>) -> Self {
        Self {
            rows: Vec::new(),
            estimates,
            next_estimate: 0,
        }
    }

    /// Append one plan row. An empty detail becomes the "-" sentinel so no cell renders blank.
    pub(crate) fn emit(&mut self, depth: i64, node: impl Into<String>, detail: impl Into<String>) {
        let mut detail = detail.into();
        if detail.is_empty() {
            detail = "-".to_string();
        }
        let (est_rows, est_cost) = self
            .estimates
            .get(self.next_estimate)
            .copied()
            .unwrap_or((0, 0));
        self.next_estimate += 1;
        self.rows.push(vec![
            Value::Int(depth),
            Value::Text(node.into()),
            Value::Text(detail),
            Value::Int(est_rows),
            Value::Int(est_cost),
        ]);
    }
}

/// Render an INSERT's ON CONFLICT disposition (or "-" when there is none).
pub(crate) fn insert_detail(ins: &Insert) -> String {
    match &ins.on_conflict {
        None => "-".to_string(),
        Some(oc) => match oc.action {
            ConflictAction::DoUpdate { .. } => "on conflict do update".to_string(),
            ConflictAction::DoNothing => "on conflict do nothing".to_string(),
        },
    }
}

/// Render a CTE binding's attributes: its materialization mode (inlined vs materialized — the
/// planner's choice) and whether it is recursive.
pub(crate) fn cte_detail(b: &CteBinding, mode: CteMode) -> String {
    let mut parts = vec![cte_mode_text(mode).to_string()];
    if b.recursive.is_some() {
        parts.push("recursive".to_string());
    }
    parts.join("; ")
}

/// Label a CTE materialization mode.
pub(crate) fn cte_mode_text(m: CteMode) -> &'static str {
    match m {
        CteMode::Materialize => "materialized",
        CteMode::Inline => "inlined",
    }
}

/// Render an Aggregate node's attributes: the grouping-key count, aggregate count, the grouping-set
/// count when there is more than one set, and the HAVING conjunct count.
pub(crate) fn agg_detail(sp: &SelectPlan) -> String {
    let mut parts = vec![format!(
        "groups={} aggs={}",
        sp.group_keys.len(),
        sp.agg_specs.len()
    )];
    if sp.group_sets.len() > 1 {
        parts.push(format!("sets={}", sp.group_sets.len()));
    }
    if let Some(having) = &sp.having {
        parts.push(format!("having:conjuncts={}", conjunct_count(having)));
    }
    parts.join("; ")
}

/// Render a Nested Loop node's attributes: the join kind and the ON predicate's conjunct count (a
/// CROSS join has no ON).
pub(crate) fn join_detail(j: &PlanJoin) -> String {
    let kind = join_kind_text(j.kind);
    match &j.on {
        None => kind.to_string(),
        Some(on) => format!("{kind}; on:conjuncts={}", conjunct_count(on)),
    }
}

/// The label for a join kind.
pub(crate) fn join_kind_text(k: JoinKind) -> &'static str {
    match k {
        JoinKind::Inner => "inner",
        JoinKind::Cross => "cross",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
    }
}

/// The node label for a set-operation kind.
pub(crate) fn set_op_node_name(op: SetOpKind) -> &'static str {
    match op {
        SetOpKind::Union => "Union",
        SetOpKind::Intersect => "Intersect",
        SetOpKind::Except => "Except",
    }
}

/// Render a set operation's ALL / DISTINCT disposition.
pub(crate) fn set_op_detail(all: bool) -> &'static str {
    if all { "all" } else { "distinct" }
}

/// Append an elided-ORDER-BY note to a node's detail (replacing a "-" sentinel).
pub(crate) fn with_note(detail: impl Into<String>, note: &str) -> String {
    let detail = detail.into();
    if note.is_empty() {
        return detail;
    }
    if detail.is_empty() || detail == "-" {
        return format!("ordered: {note}");
    }
    format!("{detail}; ordered: {note}")
}

/// Render a primary-key bound's terms as `col <op> <src>` conjuncts joined by " and " — e.g.
/// `id = $1`, `id >= 5 and id < 10`.
pub(crate) fn render_bound_terms(col: &str, terms: &[BoundTerm]) -> String {
    terms
        .iter()
        .map(|t| format!("{col} {} {}", bound_op_text(t.op), render_bound_src(&t.src)))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// The symbol for a bound comparison operator.
pub(crate) fn bound_op_text(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Gt => ">",
        CmpOp::Le => "<=",
        CmpOp::Ge => ">=",
    }
}

/// Render a bound's const-source operand: a bind parameter as `$N` (1-based), a correlated
/// outer-column reference as `outer`, or a literal. Integer / boolean / decimal literals render
/// deterministically; a text literal renders verbatim unless it contains a newline (which would split
/// the cell), in which case `<text>`; every other constant type renders `<value>` for now (a later
/// slice widens this). A `float` bound source cannot arise here — float keys do not push down, so the
/// determinism-ledger `<float>` token the Go/TS renderers reserve has no [`BoundSrc`] analogue.
pub(crate) fn render_bound_src(src: &BoundSrc) -> String {
    match src {
        BoundSrc::Param(idx) => format!("${}", idx + 1),
        BoundSrc::Outer { .. } => "outer".to_string(),
        // An index-nested-loop bound source — a column of an earlier join relation resolved per
        // outer row (cost.md §3 "JOIN"). Rendered generically (the global column index is not a
        // user-facing name, like the correlated `outer` case above).
        BoundSrc::Sibling(_) => "join".to_string(),
        BoundSrc::Int(n) => n.to_string(),
        BoundSrc::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        BoundSrc::Decimal(d) => d.render(),
        BoundSrc::Text(s) => {
            if s.contains(['\n', '\r']) {
                "<text>".to_string()
            } else {
                format!("'{s}'")
            }
        }
        BoundSrc::Uuid(_)
        | BoundSrc::Timestamp(_)
        | BoundSrc::Date(_)
        | BoundSrc::Bytea(_)
        | BoundSrc::Interval(_)
        | BoundSrc::Null => "<value>".to_string(),
    }
}

/// Count the top-level AND conjuncts of a residual filter (a deterministic integer — the plan text
/// carries the count, not the expression itself; a full expression printer is a later slice,
/// spec/design/explain.md §5).
pub(crate) fn conjunct_count(e: &RExpr) -> i64 {
    match e {
        RExpr::And(l, r) => conjunct_count(l) + conjunct_count(r),
        _ => 1,
    }
}

/// Render a Limit node's `limit=N` / `offset=M` attributes (an absent side is omitted).
pub(crate) fn limit_detail(limit: Option<i64>, offset: Option<i64>) -> String {
    let mut parts = Vec::new();
    if let Some(l) = limit {
        parts.push(format!("limit={l}"));
    }
    if let Some(o) = offset {
        parts.push(format!("offset={o}"));
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

/// Count the set entries in a touched-set mask.
pub(crate) fn count_true(mask: &[bool]) -> usize {
    mask.iter().filter(|&&b| b).count()
}

/// Whether a statement mutates the database (so autocommit must capture + durably persist it,
/// and a READ ONLY transaction must reject it — spec/design/transactions.md §4.1/§4.3). Reads
/// (`SELECT`, set operations) and transaction control run against the committed state / handle
/// state with no data mutation.
/// Map a `serial` pseudo-type name to its underlying integer scalar (spec/design/sequences.md §12) —
/// `serial`/`serial4` → i32, `bigserial`/`serial8` → i64, `smallserial`/`serial2` → i16. `None` for
/// any other name. Recognized **only** in a CREATE TABLE column-type position (the one caller); the
/// match is case-insensitive (the parser passes the type name verbatim).
pub(crate) fn serial_pseudo_type(name: &str) -> Option<ScalarType> {
    match name.to_ascii_lowercase().as_str() {
        "serial" | "serial4" => Some(ScalarType::Int32),
        "bigserial" | "serial8" => Some(ScalarType::Int64),
        "smallserial" | "serial2" => Some(ScalarType::Int16),
        _ => None,
    }
}

/// Resolve a parsed `SeqOptions` set into a validated `SequenceDef` (spec/design/sequences.md §1/§14),
/// shared by `CREATE SEQUENCE` and an IDENTITY column's `( seq_options )` (§13). The `AS` type (or the
/// `serial`/identity-supplied default) sets the default + validated bounds; then validates INCREMENT
/// (≠ 0), CACHE (≥ 1), explicit MIN/MAX within the type range, MINVALUE ≤ MAXVALUE, and START in
/// `[min, max]` (each `22023`). A fresh sequence starts with `last_value = start`, `is_called = false`.
/// `owned_by` carries the IDENTITY / `serial` owner link (`None` for a plain `CREATE SEQUENCE`).
pub(crate) fn build_sequence_def(
    name: &str,
    options: &SeqOptions,
    owned_by: Option<SeqOwner>,
) -> Result<SequenceDef> {
    // The value type (§14): `AS <type>` → the named type (22023 if not an integer type), else bigint.
    let dtype = match &options.data_type {
        Some(tn) => SeqDataType::from_type_name(tn).ok_or_else(|| {
            EngineError::new(
                SqlState::InvalidParameterValue,
                "sequence type must be smallint, integer, or bigint".to_string(),
            )
        })?,
        None => SeqDataType::BigInt,
    };
    let (type_min, type_max) = dtype.range();
    let increment = options.increment.unwrap_or(1);
    if increment == 0 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "INCREMENT must not be zero".to_string(),
        ));
    }
    let cache = options.cache.unwrap_or(1);
    if cache < 1 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("CACHE ({cache}) must be greater than zero"),
        ));
    }
    let (def_min, def_max) = dtype.default_bounds(increment);
    // An explicit MAXVALUE/MINVALUE outside the type range is 22023 — checked (MAX first, PG order)
    // BEFORE the MIN > MAX consistency check (§14.2).
    if let Some(Some(v)) = options.max_value {
        if v > type_max {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!(
                    "MAXVALUE ({v}) is out of range for sequence data type {}",
                    dtype.pg_name()
                ),
            ));
        }
    }
    if let Some(Some(v)) = options.min_value {
        if v < type_min {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!(
                    "MINVALUE ({v}) is out of range for sequence data type {}",
                    dtype.pg_name()
                ),
            ));
        }
    }
    // `Some(Some(v))` MINVALUE v / `Some(None)` NO MINVALUE / `None` unset → the type default.
    let min_value = match options.min_value {
        Some(Some(v)) => v,
        Some(None) | None => def_min,
    };
    let max_value = match options.max_value {
        Some(Some(v)) => v,
        Some(None) | None => def_max,
    };
    // PG requires MINVALUE strictly less than MAXVALUE (a one-value sequence is rejected); jed
    // previously allowed `==` — corrected here so CREATE and ALTER (sequences.md §15.2) agree with PG.
    if min_value >= max_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("MINVALUE ({min_value}) must be less than MAXVALUE ({max_value})"),
        ));
    }
    // START defaults to MINVALUE (ascending) / MAXVALUE (descending) and must lie in [min, max].
    let start = options
        .start
        .unwrap_or(if increment < 0 { max_value } else { min_value });
    seq_bound_check_start(start, min_value, max_value)?;
    Ok(SequenceDef {
        name: name.to_string(),
        increment,
        min_value,
        max_value,
        start,
        cache,
        cycle: options.cycle.unwrap_or(false),
        last_value: start,
        is_called: false,
        owned_by,
    })
}

/// PG's START-in-bounds cross-check (`init_params`): `start ∈ [min, max]`, else 22023 with PG's
/// wording. Shared by `CREATE` (build_sequence_def) and `ALTER` (apply_seq_alter).
pub(crate) fn seq_bound_check_start(start: i64, min_value: i64, max_value: i64) -> Result<()> {
    if start < min_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("START value ({start}) cannot be less than MINVALUE ({min_value})"),
        ));
    }
    if start > max_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("START value ({start}) cannot be greater than MAXVALUE ({max_value})"),
        ));
    }
    Ok(())
}

/// PG's last_value (RESTART) cross-check (`init_params`): the post-edit `last_value ∈ [min, max]`,
/// else 22023. PG uses the "RESTART value …" wording even when no `RESTART` was written (§15.2).
pub(crate) fn seq_bound_check_last(last_value: i64, min_value: i64, max_value: i64) -> Result<()> {
    if last_value < min_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("RESTART value ({last_value}) cannot be less than MINVALUE ({min_value})"),
        ));
    }
    if last_value > max_value {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("RESTART value ({last_value}) cannot be greater than MAXVALUE ({max_value})"),
        ));
    }
    Ok(())
}

/// Re-edit an existing `SequenceDef` per `ALTER SEQUENCE s <options>` (spec/design/sequences.md §15.2)
/// — PG's `init_params` with `isInit = false`. Only the **written** options change; `last_value`/
/// `is_called` are preserved unless `restart` is given. `restart` is `None` (no `RESTART`),
/// `Some(None)` (bare `RESTART` → the stored `START`), or `Some(Some(n))` (`RESTART WITH n`). The
/// value type is not persisted (§14.4), so `NO MINVALUE`/`NO MAXVALUE` reset the open direction to the
/// bigint bound and an explicit bound is range-checked only by `i64` — a documented divergence for a
/// typed sequence. `data_type` must be `None` (the caller rejects `AS` as 0A000 first).
pub(crate) fn apply_seq_alter(
    existing: &SequenceDef,
    options: &SeqOptions,
    restart: Option<Option<i64>>,
) -> Result<SequenceDef> {
    debug_assert!(
        options.data_type.is_none(),
        "ALTER ... AS is rejected 0A000 by the caller"
    );
    let mut def = existing.clone();
    if let Some(inc) = options.increment {
        if inc == 0 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                "INCREMENT must not be zero".to_string(),
            ));
        }
        def.increment = inc;
    }
    if let Some(c) = options.cache {
        if c < 1 {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("CACHE ({c}) must be greater than zero"),
            ));
        }
        def.cache = c;
    }
    // `NO MINVALUE`/`NO MAXVALUE` recompute the default for the (possibly new) INCREMENT sign — but
    // against the bigint range, since the value type is not persisted (§14.4). An explicit bound is
    // taken as written (i64-bounded only). An unwritten bound is preserved (PG keeps it even when the
    // INCREMENT sign flips — sequences.md §15.2).
    let (def_min, def_max) = SeqDataType::BigInt.default_bounds(def.increment);
    match options.min_value {
        Some(Some(v)) => def.min_value = v,
        Some(None) => def.min_value = def_min,
        None => {}
    }
    match options.max_value {
        Some(Some(v)) => def.max_value = v,
        Some(None) => def.max_value = def_max,
        None => {}
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
    if let Some(s) = options.start {
        def.start = s;
    }
    // Cross-check 1: START ∈ [min, max].
    seq_bound_check_start(def.start, def.min_value, def.max_value)?;
    // RESTART (applied last, before the last_value cross-check).
    match restart {
        Some(Some(n)) => {
            def.last_value = n;
            def.is_called = false;
        }
        Some(None) => {
            def.last_value = def.start;
            def.is_called = false;
        }
        None => {}
    }
    // Cross-check 2: the preserved/restarted last_value ∈ [min, max].
    seq_bound_check_last(def.last_value, def.min_value, def.max_value)?;
    if let Some(c) = options.cycle {
        def.cycle = c;
    }
    Ok(def)
}

pub(crate) fn stmt_is_write(stmt: &Statement) -> bool {
    // EXPLAIN is a read: plain EXPLAIN plans without executing (even of a DML inner — it never
    // mutates). Only EXPLAIN ANALYZE runs the inner statement, so it is a write iff the inner is
    // (spec/design/explain.md §3).
    if let Statement::Explain { analyze, inner } = stmt {
        return *analyze && stmt_is_write(inner);
    }
    matches!(
        stmt,
        Statement::Analyze(_)
            | Statement::CreateTable(_)
            | Statement::DropTable(_)
            | Statement::AlterTable(_)
            | Statement::CreateIndex(_)
            | Statement::DropIndex(_)
            | Statement::CreateType(_)
            | Statement::DropType(_)
            | Statement::CreateSequence(_)
            | Statement::AlterSequence(_)
            | Statement::DropSequence(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
    )
    // A WITH statement with any data-modifying part is a write (it stages INSERT/UPDATE/DELETE
    // effects — writable-cte.md): it must take the write gate, accumulate into `working`, and commit.
    || matches!(stmt, Statement::With(wq) if with_has_dml(wq))
    // A read-shaped statement that calls a sequence-mutating function (nextval/setval) IS a write
    // (spec/design/sequences.md §4): it must take the write gate, stage the advance, and commit
    // (autocommit) — and is 25006 in a READ ONLY transaction, exactly like any other write.
    || stmt_calls_seq_mutator(stmt)
}

/// Whether `stmt`'s expression trees contain a sequence-MUTATING function call (`nextval`; in S2,
/// `setval`) anywhere — which makes an otherwise read-shaped statement a write (sequences.md §4).
/// Only the **read-shaped** statements need checking: INSERT/UPDATE/DELETE/DDL are already writes
/// (the `matches!` in [`stmt_is_write`] short-circuits before this), and an INSERT `VALUES` slot is
/// literal-only (no function call). `currval` is a pure read and is NOT counted. The `Expr` walk is
/// exhaustive (the compiler enforces it), so no expression position is missed.
pub(crate) fn stmt_calls_seq_mutator(stmt: &Statement) -> bool {
    match stmt {
        Statement::Select(s) => select_calls_seq_mutator(s),
        Statement::SetOp(so) => setop_calls_seq_mutator(so),
        Statement::With(w) => {
            w.ctes.iter().any(|c| cte_body_calls_seq_mutator(&c.body))
                || cte_body_calls_seq_mutator(&w.body)
        }
        _ => false,
    }
}

/// Whether a `cte_body` calls a sequence-mutating function. A query body delegates to the query
/// walk; a data-modifying body already makes the `WITH` a write (via [`with_has_dml`]), so this is
/// not reached for it — it is treated as a write regardless (writable-cte.md).
pub(crate) fn cte_body_calls_seq_mutator(body: &CteBody) -> bool {
    match body {
        CteBody::Query(q) => query_calls_seq_mutator(q),
        _ => true,
    }
}

pub(crate) fn query_calls_seq_mutator(qe: &QueryExpr) -> bool {
    match qe {
        QueryExpr::Select(s) => select_calls_seq_mutator(s),
        QueryExpr::SetOp(so) => setop_calls_seq_mutator(so),
        // A nested `WITH`'s CTE bodies and main body may call a sequence mutator (cte.md §7).
        QueryExpr::With(we) => {
            we.ctes.iter().any(|c| cte_body_calls_seq_mutator(&c.body))
                || query_calls_seq_mutator(&we.body)
        }
    }
}

pub(crate) fn setop_calls_seq_mutator(so: &SetOp) -> bool {
    query_calls_seq_mutator(&so.lhs) || query_calls_seq_mutator(&so.rhs)
}

pub(crate) fn select_calls_seq_mutator(s: &Select) -> bool {
    let item_calls = match &s.items {
        SelectItems::All => false,
        SelectItems::Items(items) => items.iter().any(|i| expr_calls_seq_mutator(&i.expr)),
    };
    item_calls
        || s.from.as_ref().is_some_and(table_ref_calls)
        || s.joins
            .iter()
            .any(|j| table_ref_calls(&j.table) || j.on.as_ref().is_some_and(expr_calls_seq_mutator))
        || s.filter.as_ref().is_some_and(expr_calls_seq_mutator)
        || s.group_by.iter().any(|item| {
            let mut found = false;
            item.for_each_expr(&mut |e| found |= expr_calls_seq_mutator(e));
            found
        })
        || s.having.as_ref().is_some_and(expr_calls_seq_mutator)
}

pub(crate) fn table_ref_calls(t: &TableRef) -> bool {
    t.args
        .as_ref()
        .is_some_and(|a| a.iter().any(expr_calls_seq_mutator))
        || t.subquery
            .as_ref()
            .is_some_and(|q| query_calls_seq_mutator(q))
        || t.values
            .as_ref()
            .is_some_and(|rows| rows.iter().flatten().any(expr_calls_seq_mutator))
}

/// Exhaustive over `Expr` (the compiler enforces it): true iff the tree contains a sequence-
/// mutating call (`nextval` or `setval`).
pub(crate) fn expr_calls_seq_mutator(e: &Expr) -> bool {
    match e {
        Expr::FuncCall { name, args, .. } => {
            name.eq_ignore_ascii_case("nextval")
                || name.eq_ignore_ascii_case("setval")
                || args.iter().any(expr_calls_seq_mutator)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => false,
        Expr::Row(es) | Expr::Array(es) => es.iter().any(expr_calls_seq_mutator),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_calls_seq_mutator(base),
        Expr::QualifiedStar { .. } => false,
        Expr::Subscript { base, subscripts } => {
            expr_calls_seq_mutator(base)
                || subscripts.iter().any(|s| match s {
                    SubscriptSpec::Index(x) => expr_calls_seq_mutator(x),
                    SubscriptSpec::Slice(lo, hi) => {
                        lo.as_ref().is_some_and(|x| expr_calls_seq_mutator(x))
                            || hi.as_ref().is_some_and(|x| expr_calls_seq_mutator(x))
                    }
                })
        }
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Unary { operand: inner, .. } => expr_calls_seq_mutator(inner),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_calls_seq_mutator(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            expr_calls_seq_mutator(ctx) || expr_calls_seq_mutator(path)
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            expr_calls_seq_mutator(lhs) || expr_calls_seq_mutator(rhs)
        }
        Expr::In { lhs, list, .. } => {
            expr_calls_seq_mutator(lhs) || list.iter().any(expr_calls_seq_mutator)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            expr_calls_seq_mutator(lhs) || expr_calls_seq_mutator(lo) || expr_calls_seq_mutator(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_ref().is_some_and(|x| expr_calls_seq_mutator(x))
                || whens
                    .iter()
                    .any(|(c, r)| expr_calls_seq_mutator(c) || expr_calls_seq_mutator(r))
                || els.as_ref().is_some_and(|x| expr_calls_seq_mutator(x))
        }
        Expr::Coalesce(args) => args.iter().any(expr_calls_seq_mutator),
        Expr::GreatestLeast { args, .. } => args.iter().any(expr_calls_seq_mutator),
        Expr::ScalarSubquery(q) | Expr::Exists(q) => query_calls_seq_mutator(q),
        Expr::InSubquery { lhs, query, .. } | Expr::QuantifiedSubquery { lhs, query, .. } => {
            expr_calls_seq_mutator(lhs) || query_calls_seq_mutator(query)
        }
        Expr::Quantified { lhs, array, .. } => {
            expr_calls_seq_mutator(lhs) || expr_calls_seq_mutator(array)
        }
    }
}

/// The privilege requirements collected from one statement (spec/design/session.md §5.3): the
/// per-table privileges, the named functions (each needs `EXECUTE`), and whether the statement is
/// DDL (gated by `allow_ddl`). Collected by an exhaustive AST walk (the `Expr` arm is compiler-
/// enforced exhaustive, mirroring [`expr_calls_seq_mutator`]).
#[derive(Default)]
pub(crate) struct PrivReq {
    /// `(table name, required privilege)` in source-walk order; deduplication is unnecessary (the
    /// check is idempotent and a fully-permissive session never reaches the walk).
    pub(crate) tables: Vec<(String, Privilege)>,
    /// Named functions called (each requires `EXECUTE`), in source-walk order.
    pub(crate) functions: Vec<String>,
    /// Whether the statement is DDL (CREATE / DROP / ALTER) — gated by `allow_ddl`.
    pub(crate) is_ddl: bool,
    /// Whether the DDL targets a SESSION-LOCAL temporary table (`CREATE TEMP TABLE`) — gated by
    /// `allow_temp_ddl` instead of `allow_ddl` (spec/design/temp-tables.md §5). Set for a `CREATE
    /// TEMP`; a `DROP` is classified by resolving the name in `check_privileges`.
    pub(crate) is_temp_ddl: bool,
}

impl PrivReq {
    pub(crate) fn need_table(&mut self, name: &str, p: Privilege) {
        self.tables.push((name.to_string(), p));
    }
    pub(crate) fn need_function(&mut self, name: &str) {
        self.functions.push(name.to_string());
    }
}

/// Collect the privilege requirements of `stmt` (spec/design/session.md §5.3). Transaction control
/// carries none (it is handled before dispatch); DDL just sets `is_ddl`.
pub(crate) fn collect_stmt_privs(stmt: &Statement, req: &mut PrivReq) {
    let locals = HashSet::new();
    match stmt {
        Statement::Analyze(analyze) => {
            req.is_ddl = true;
            req.need_table(&analyze.name, Privilege::Select);
        }
        Statement::CreateTable(ct) => {
            req.is_ddl = true;
            // A temp table's DDL is gated by the temp-scoped split of `allow_ddl` (temp-tables.md §5):
            // `allow_temp_ddl` for a session-local temp table.
            req.is_temp_ddl = ct.temp;
        }
        Statement::DropTable(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::CreateType(_)
        | Statement::DropType(_)
        | Statement::CreateSequence(_)
        | Statement::DropSequence(_)
        | Statement::AlterSequence(_)
        | Statement::AlterTable(_) => req.is_ddl = true,
        Statement::Insert(ins) => collect_insert_privs(ins, req, &locals),
        Statement::Select(sel) => collect_select_privs(sel, req, &locals),
        Statement::SetOp(so) => collect_setop_privs(so, req, &locals),
        Statement::With(wq) => collect_with_privs(wq, req, &locals),
        Statement::Update(upd) => collect_update_privs(upd, req, &locals),
        Statement::Delete(del) => collect_delete_privs(del, req, &locals),
        // EXPLAIN requires the inner statement's privileges (EXPLAIN INSERT needs INSERT, matching
        // PG). Plain EXPLAIN never executes, but authorization is checked on the inner regardless
        // (spec/design/explain.md §1).
        Statement::Explain { inner, .. } => collect_stmt_privs(inner, req),
        Statement::Begin { .. } | Statement::Commit | Statement::Rollback => {}
    }
}

pub(crate) fn collect_insert_privs(ins: &Insert, req: &mut PrivReq, locals: &HashSet<String>) {
    // The write target needs INSERT. A bare `INSERT … VALUES` reads nothing (the slots are literals
    // / params), so it needs only INSERT; an `INSERT … SELECT` source needs SELECT on its tables.
    req.need_table(&ins.table, Privilege::Insert);
    if let InsertSource::Select(sel) = &ins.source {
        collect_select_privs(sel, req, locals);
    }
    if let Some(oc) = &ins.on_conflict {
        if let ConflictAction::DoUpdate {
            assignments,
            filter,
        } = &oc.action
        {
            for a in assignments {
                collect_expr_privs(&a.value, req, locals);
            }
            if let Some(f) = filter {
                collect_expr_privs(f, req, locals);
            }
        }
    }
    collect_items_privs(&ins.returning, req, locals);
}

pub(crate) fn collect_update_privs(upd: &Update, req: &mut PrivReq, locals: &HashSet<String>) {
    req.need_table(&upd.table, Privilege::Update);
    // SELECT on the target if it reads any column — a WHERE, a RETURNING, or a column/subquery-
    // referencing assignment RHS (a constant-only `SET a = 1` with no WHERE/RETURNING reads nothing).
    let reads = upd.filter.is_some()
        || upd.returning.is_some()
        || upd.assignments.iter().any(|a| expr_reads_columns(&a.value));
    if reads {
        req.need_table(&upd.table, Privilege::Select);
    }
    for a in &upd.assignments {
        collect_expr_privs(&a.value, req, locals);
    }
    if let Some(f) = &upd.filter {
        collect_expr_privs(f, req, locals);
    }
    collect_items_privs(&upd.returning, req, locals);
}

pub(crate) fn collect_delete_privs(del: &Delete, req: &mut PrivReq, locals: &HashSet<String>) {
    req.need_table(&del.table, Privilege::Delete);
    // DELETE reads the target's columns through a WHERE or a RETURNING.
    if del.filter.is_some() || del.returning.is_some() {
        req.need_table(&del.table, Privilege::Select);
    }
    if let Some(f) = &del.filter {
        collect_expr_privs(f, req, locals);
    }
    collect_items_privs(&del.returning, req, locals);
}

pub(crate) fn collect_query_privs(qe: &QueryExpr, req: &mut PrivReq, locals: &HashSet<String>) {
    match qe {
        QueryExpr::Select(s) => collect_select_privs(s, req, locals),
        QueryExpr::SetOp(so) => collect_setop_privs(so, req, locals),
        // A nested `WITH` establishes its own CTE scope (spec/design/cte.md §7): the enclosing
        // locals are NOT inherited (an enclosing CTE name resolves to a base table inside, so it is
        // privilege-checked), and the nested CTE names shadow base tables only within this node.
        QueryExpr::With(we) => {
            let mut scope = HashSet::new();
            for cte in &we.ctes {
                collect_cte_body_privs(&cte.body, req, &scope);
                scope.insert(cte.name.to_ascii_lowercase());
            }
            collect_query_privs(&we.body, req, &scope);
        }
    }
}

pub(crate) fn collect_setop_privs(so: &SetOp, req: &mut PrivReq, locals: &HashSet<String>) {
    collect_query_privs(&so.lhs, req, locals);
    collect_query_privs(&so.rhs, req, locals);
}

pub(crate) fn collect_with_privs(wq: &WithQuery, req: &mut PrivReq, locals: &HashSet<String>) {
    // A CTE name shadows a base table inside the WITH (a `FROM <cte>` is not a catalog object), so
    // it is added to the local scope and never privilege-checked. Forward-only visibility: each CTE
    // body sees the CTE names declared before it. A data-modifying body / primary needs the write
    // privilege on its target table (writable-cte.md).
    let mut scope = locals.clone();
    for cte in &wq.ctes {
        collect_cte_body_privs(&cte.body, req, &scope);
        scope.insert(cte.name.to_ascii_lowercase());
    }
    collect_cte_body_privs(&wq.body, req, &scope);
}

/// Collect the privilege requirements of a `cte_body` — a query, or a data-modifying statement
/// (spec/design/writable-cte.md) which needs the write privilege on its target.
pub(crate) fn collect_cte_body_privs(body: &CteBody, req: &mut PrivReq, locals: &HashSet<String>) {
    match body {
        CteBody::Query(q) => collect_query_privs(q, req, locals),
        CteBody::Insert(ins) => collect_insert_privs(ins, req, locals),
        CteBody::Update(upd) => collect_update_privs(upd, req, locals),
        CteBody::Delete(del) => collect_delete_privs(del, req, locals),
    }
}

pub(crate) fn collect_select_privs(s: &Select, req: &mut PrivReq, locals: &HashSet<String>) {
    if let Some(from) = &s.from {
        collect_table_ref_privs(from, req, locals);
    }
    for j in &s.joins {
        collect_table_ref_privs(&j.table, req, locals);
        if let Some(on) = &j.on {
            collect_expr_privs(on, req, locals);
        }
    }
    if let SelectItems::Items(items) = &s.items {
        for it in items {
            collect_expr_privs(&it.expr, req, locals);
        }
    }
    if let Some(f) = &s.filter {
        collect_expr_privs(f, req, locals);
    }
    for item in &s.group_by {
        item.for_each_expr(&mut |g| collect_expr_privs(g, req, locals));
    }
    if let Some(h) = &s.having {
        collect_expr_privs(h, req, locals);
    }
}

pub(crate) fn collect_table_ref_privs(t: &TableRef, req: &mut PrivReq, locals: &HashSet<String>) {
    if let Some(args) = &t.args {
        // A set-returning function used as a row source — EXECUTE on the function; its args are exprs.
        req.need_function(&t.name);
        for a in args {
            collect_expr_privs(a, req, locals);
        }
    } else if let Some(sub) = &t.subquery {
        collect_query_privs(sub, req, locals);
    } else if let Some(rows) = &t.values {
        for e in rows.iter().flatten() {
            collect_expr_privs(e, req, locals);
        }
    } else if !locals.contains(&t.name.to_ascii_lowercase()) {
        // A base-table reference (not a CTE / derived-table label) — needs SELECT.
        req.need_table(&t.name, Privilege::Select);
    }
}

pub(crate) fn collect_items_privs(
    items: &Option<SelectItems>,
    req: &mut PrivReq,
    locals: &HashSet<String>,
) {
    if let Some(SelectItems::Items(list)) = items {
        for it in list {
            collect_expr_privs(&it.expr, req, locals);
        }
    }
}

/// Exhaustive over `Expr` (compiler-enforced, mirroring [`expr_calls_seq_mutator`]): collect every
/// named function call (`EXECUTE`) and walk every subquery (its tables need `SELECT`).
pub(crate) fn collect_expr_privs(e: &Expr, req: &mut PrivReq, locals: &HashSet<String>) {
    match e {
        Expr::FuncCall { name, args, .. } => {
            req.need_function(name);
            for a in args {
                collect_expr_privs(a, req, locals);
            }
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => {}
        Expr::Row(es) | Expr::Array(es) => {
            for x in es {
                collect_expr_privs(x, req, locals);
            }
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => {
            collect_expr_privs(base, req, locals)
        }
        // `t.*` names a relation already in FROM — its SELECT privilege is required by the FROM
        // clause itself, so the star adds no new function/table privilege here.
        Expr::QualifiedStar { .. } => {}
        Expr::Subscript { base, subscripts } => {
            collect_expr_privs(base, req, locals);
            for s in subscripts {
                match s {
                    SubscriptSpec::Index(x) => collect_expr_privs(x, req, locals),
                    SubscriptSpec::Slice(lo, hi) => {
                        if let Some(x) = lo {
                            collect_expr_privs(x, req, locals);
                        }
                        if let Some(x) = hi {
                            collect_expr_privs(x, req, locals);
                        }
                    }
                }
            }
        }
        Expr::Cast { inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. } => collect_expr_privs(inner, req, locals),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => collect_expr_privs(operand, req, locals),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => {
            collect_expr_privs(ctx, req, locals);
            collect_expr_privs(path, req, locals);
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            collect_expr_privs(lhs, req, locals);
            collect_expr_privs(rhs, req, locals);
        }
        Expr::In { lhs, list, .. } => {
            collect_expr_privs(lhs, req, locals);
            for x in list {
                collect_expr_privs(x, req, locals);
            }
        }
        Expr::Between { lhs, lo, hi, .. } => {
            collect_expr_privs(lhs, req, locals);
            collect_expr_privs(lo, req, locals);
            collect_expr_privs(hi, req, locals);
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(x) = operand {
                collect_expr_privs(x, req, locals);
            }
            for (c, r) in whens {
                collect_expr_privs(c, req, locals);
                collect_expr_privs(r, req, locals);
            }
            if let Some(x) = els {
                collect_expr_privs(x, req, locals);
            }
        }
        Expr::Coalesce(args) => {
            for a in args {
                collect_expr_privs(a, req, locals);
            }
        }
        Expr::GreatestLeast { args, .. } => {
            for a in args {
                collect_expr_privs(a, req, locals);
            }
        }
        Expr::ScalarSubquery(q) | Expr::Exists(q) => collect_query_privs(q, req, locals),
        Expr::InSubquery { lhs, query, .. } | Expr::QuantifiedSubquery { lhs, query, .. } => {
            collect_expr_privs(lhs, req, locals);
            collect_query_privs(query, req, locals);
        }
        Expr::Quantified { lhs, array, .. } => {
            collect_expr_privs(lhs, req, locals);
            collect_expr_privs(array, req, locals);
        }
    }
}

/// Whether `e` reads a stored column or a subquery's rows — the trigger for an UPDATE's `SELECT`
/// requirement on its target (spec/design/session.md §5.3). A column reference (`Column` /
/// `QualifiedColumn` / a field/subscript over one) or any subquery counts; a pure constant /
/// parameter expression does not. Exhaustive over `Expr` (compiler-enforced).
pub(crate) fn expr_reads_columns(e: &Expr) -> bool {
    match e {
        Expr::Column(_) | Expr::QualifiedColumn { .. } => true,
        Expr::ScalarSubquery(_) | Expr::Exists(_) => true,
        Expr::Literal(_) | Expr::TypedLiteral { .. } | Expr::Param(_) => false,
        Expr::Row(es) | Expr::Array(es) => es.iter().any(expr_reads_columns),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_reads_columns(base),
        // `t.*` reads the relation's columns (e.g. `RETURNING t.*`).
        Expr::QualifiedStar { .. } => true,
        Expr::Subscript { base, subscripts } => {
            expr_reads_columns(base)
                || subscripts.iter().any(|s| match s {
                    SubscriptSpec::Index(x) => expr_reads_columns(x),
                    SubscriptSpec::Slice(lo, hi) => {
                        lo.as_ref().is_some_and(|x| expr_reads_columns(x))
                            || hi.as_ref().is_some_and(|x| expr_reads_columns(x))
                    }
                })
        }
        Expr::Cast { inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. } => expr_reads_columns(inner),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_reads_columns(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => expr_reads_columns(ctx) || expr_reads_columns(path),
        Expr::FuncCall { args, .. } => args.iter().any(expr_reads_columns),
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => expr_reads_columns(lhs) || expr_reads_columns(rhs),
        Expr::In { lhs, list, .. } => {
            expr_reads_columns(lhs) || list.iter().any(expr_reads_columns)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            expr_reads_columns(lhs) || expr_reads_columns(lo) || expr_reads_columns(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_ref().is_some_and(|x| expr_reads_columns(x))
                || whens
                    .iter()
                    .any(|(c, r)| expr_reads_columns(c) || expr_reads_columns(r))
                || els.as_ref().is_some_and(|x| expr_reads_columns(x))
        }
        Expr::Coalesce(args) => args.iter().any(expr_reads_columns),
        Expr::GreatestLeast { args, .. } => args.iter().any(expr_reads_columns),
        Expr::InSubquery { .. } | Expr::QuantifiedSubquery { .. } => true,
        Expr::Quantified { lhs, array, .. } => expr_reads_columns(lhs) || expr_reads_columns(array),
    }
}

/// A short label for a statement kind, for the 25006 read-only-violation message (the message
/// text is informational — never matched; spec/design/conformance.md §2).
pub(crate) fn stmt_kind(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::Analyze(_) => "ANALYZE",
        Statement::CreateTable(_) => "CREATE TABLE",
        Statement::DropTable(_) => "DROP TABLE",
        Statement::AlterTable(_) => "ALTER TABLE",
        Statement::CreateIndex(_) => "CREATE INDEX",
        Statement::DropIndex(_) => "DROP INDEX",
        Statement::CreateType(_) => "CREATE TYPE",
        Statement::DropType(_) => "DROP TYPE",
        Statement::CreateSequence(_) => "CREATE SEQUENCE",
        Statement::AlterSequence(_) => "ALTER SEQUENCE",
        Statement::DropSequence(_) => "DROP SEQUENCE",
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Select(_) | Statement::SetOp(_) | Statement::With(_) => "SELECT",
        Statement::Explain { .. } => "EXPLAIN",
        Statement::Begin { .. } => "BEGIN",
        Statement::Commit => "COMMIT",
        Statement::Rollback => "ROLLBACK",
    }
}

/// The resolved (static) type of a column of (possibly composite) declared type `ty`, resolving a
/// composite reference against the database's type catalog (spec/design/composite.md §5). Recurses
/// for nested composites; the lookup always succeeds (`validate_composite_types` proved it).
pub(crate) fn resolved_type_of_col(ty: &Type, db: &Engine) -> ResolvedType {
    match ty {
        Type::Scalar(s) => resolved_type_of(*s),
        Type::Composite(r) => {
            let def = db
                .composite_type(&r.name)
                .expect("composite type reference resolved at load / CREATE TYPE");
            let fields = def
                .fields
                .iter()
                .map(|f| (f.name.clone(), resolved_type_of_col(&f.ty, db)))
                .collect();
            ResolvedType::Composite(Box::new(CompositeRType {
                name: Some(def.name.clone()),
                fields,
            }))
        }
        Type::Array(elem) => ResolvedType::Array(Box::new(resolved_type_of_col(elem, db))),
        Type::Range(elem) => ResolvedType::Range(Box::new(resolved_type_of_col(elem, db))),
    }
}

/// The resolved (static) type of a column of scalar type `ty`.
pub(crate) fn resolved_type_of(ty: ScalarType) -> ResolvedType {
    if ty.is_text() {
        ResolvedType::Text
    } else if ty.is_bool() {
        ResolvedType::Bool
    } else if ty.is_decimal() {
        ResolvedType::Decimal
    } else if ty.is_bytea() {
        ResolvedType::Bytea
    } else if ty.is_uuid() {
        ResolvedType::Uuid
    } else if ty.is_timestamp() {
        ResolvedType::Timestamp
    } else if ty.is_timestamptz() {
        ResolvedType::Timestamptz
    } else if ty.is_interval() {
        ResolvedType::Interval
    } else if ty.is_date() {
        ResolvedType::Date
    } else if ty.is_json() {
        ResolvedType::Json
    } else if ty.is_jsonb() {
        ResolvedType::Jsonb
    } else if ty.is_float() {
        ResolvedType::Float(ty)
    } else {
        ResolvedType::Int(ty)
    }
}

/// Resolve one `Expr` into an `RExpr` plus its static type, against the FROM `scope`. `ctx`
/// is the type an untyped integer literal should adapt to (spec/design/types.md §6); `None`
/// defaults a bare literal to i64. A column reference resolves to a flat row index via the
/// scope — a bare name ambiguous across relations is 42702, an unknown qualifier is 42P01
/// (spec/design/grammar.md §15).
/// Turn a chain resolution into a resolved node + type. A `Local` column obeys the grouping
/// rule (a synthetic-slot reference in an aggregate projection, else 42803). An `Outer`
/// (correlated) reference is a per-outer-row CONSTANT, so it bypasses the grouping rule and
/// resolves to an `OuterColumn` reading the enclosing row at eval; its type is the ancestor
/// column's (spec/design/grammar.md §26).
pub(crate) fn resolve_column_ref(
    scope: &Scope,
    agg: &AggCtx,
    r: Resolved,
    name: &str,
) -> Result<(RExpr, ResolvedType)> {
    match r {
        Resolved::Local(idx) => collect_column(scope, agg, idx, name),
        Resolved::Outer { level, index } => {
            let ty = resolved_type_of_col(&scope.column_of(r).ty, scope.catalog);
            Ok((RExpr::OuterColumn { level, index }, ty))
        }
    }
}

/// Resolve a composite field selection `base.field` (spec/design/composite.md §S4) given the
/// already-resolved `base` node and its static type: `base` must be composite — else 42809
/// (wrong_object_type, PG's "column notation applied to non-composite") — and `field` must name
/// one of its fields case-insensitively (PG folds the identifier), else 42703 (undefined_column).
/// Returns the `RExpr::Field` node carrying the fixed field ordinal, plus the field's static type.
pub(crate) fn resolve_field_of(
    base_node: RExpr,
    base_ty: ResolvedType,
    field: &str,
) -> Result<(RExpr, ResolvedType)> {
    let c = match base_ty {
        ResolvedType::Composite(c) => c,
        other => {
            return Err(EngineError::new(
                SqlState::WrongObjectType,
                format!(
                    "column notation .{field} applied to type {}, which is not a composite type",
                    other.type_name()
                ),
            ));
        }
    };
    match c
        .fields
        .iter()
        .position(|(n, _)| n.eq_ignore_ascii_case(field))
    {
        Some(idx) => {
            let fty = c.fields[idx].1.clone();
            Ok((
                RExpr::Field {
                    base: Box::new(base_node),
                    index: idx,
                },
                fty,
            ))
        }
        None => Err(undefined_column(field)),
    }
}

/// Plan a subquery operand against the scope chain (spec/design/grammar.md §26). Rejects a
/// non-SELECT context (UPDATE/DELETE/INSERT — `allow_subquery=false`) with 0A000. A `$N` inside
/// the subquery is allowed: the shared `params` table is threaded into the inner plan, so a
/// parameter typed by an inner context (`WHERE inner.col = $1`) infers statement-wide and is
/// unified with any outer use of the same `$N`. A parameter with **no** type context anywhere
/// stays uninferred and `finalize` raises 42P18 (a documented divergence from PostgreSQL, which
/// defaults such a `$N` to text — grammar.md §26). The inner query is resolved ONCE, with `scope`
/// as its parent, so correlated references become `OuterColumn` and errors fire even over an
/// empty outer.
pub(crate) fn plan_subquery(
    scope: &Scope,
    inner: &QueryExpr,
    params: &mut ParamTypes,
) -> Result<QueryPlan> {
    if !scope.allow_subquery {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "subqueries are only supported in a SELECT statement",
        ));
    }
    // Any subquery makes the enclosing plan un-cacheable: the fold pass rewrites an uncorrelated one
    // (or an uncorrelated one nested inside a correlated one) into a constant using THIS execution's
    // bound params, so a reused plan would carry another execution's folded constants. Every subquery
    // form (scalar / EXISTS / IN / quantified) funnels through here.
    params.uncacheable = true;
    scope
        .catalog
        .plan_query(inner, Some(scope), scope.ctes, params)
}

/// Resolve one array-subscript bound to an integer `RExpr` (a literal adapts to int4; a non-integer
/// is 42804). A NULL-typed bound is accepted — it evaluates to a NULL subscript → NULL result.
pub(crate) fn resolve_subscript_int(
    scope: &Scope,
    e: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<RExpr> {
    let (node, ty) = resolve(scope, e, Some(ScalarType::Int32), agg, params)?;
    if !matches!(ty, ResolvedType::Int(_) | ResolvedType::Null) {
        return Err(type_error(format!(
            "array subscript must be an integer, not {}",
            ty.type_name()
        )));
    }
    Ok(node)
}

/// Find GREATEST/LEAST's common type (grammar.md §52). Unlike the CASE unifier this must yield a
/// type the fold can actually ORDER, so it (a) promotes numerics like CASE (integer widths widen,
/// int + decimal → decimal), (b) promotes float widths to the widest (the float island — a float
/// never mixes with int/decimal), and (c) requires structural equality for every other family
/// (text, bytea, uuid, the datetimes, arrays/ranges/composites/jsonb). The caller gates the result
/// through `classify_comparable`, so a non-orderable common type (json/jsonpath) still fails there.
fn unify_minmax_types(types: &[ResolvedType], name: &str) -> Result<ResolvedType> {
    let non_null: Vec<&ResolvedType> = types.iter().filter(|t| **t != ResolvedType::Null).collect();
    let Some(&first) = non_null.first() else {
        // Every argument is NULL/untyped — PostgreSQL types an all-unknown GREATEST/LEAST as text.
        return Ok(ResolvedType::Text);
    };
    if non_null
        .iter()
        .all(|t| matches!(t, ResolvedType::Int(_) | ResolvedType::Decimal))
    {
        if non_null.iter().any(|t| **t == ResolvedType::Decimal) {
            return Ok(ResolvedType::Decimal);
        }
        let mut acc = first.clone();
        for t in &non_null[1..] {
            acc = ResolvedType::Int(promote(&acc, t));
        }
        return Ok(acc);
    }
    if non_null.iter().all(|t| matches!(t, ResolvedType::Float(_))) {
        let wide = non_null
            .iter()
            .any(|t| matches!(**t, ResolvedType::Float(ScalarType::Float64)));
        return Ok(ResolvedType::Float(if wide {
            ScalarType::Float64
        } else {
            ScalarType::Float32
        }));
    }
    if non_null[1..].iter().any(|t| **t != *first) {
        return Err(type_error(format!(
            "{} types must be compatible",
            name.to_ascii_uppercase()
        )));
    }
    Ok(first.clone())
}

pub(crate) fn resolve(
    scope: &Scope,
    e: &Expr,
    ctx: Option<ScalarType>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // GROUP BY a general expression (aggregates.md §15): a non-column expression that structurally
    // matches a grouping-expression key resolves to that group's synthetic key slot — so `SELECT
    // a+b … GROUP BY a+b` projects the grouped value, like a grouping column. Columns keep their own
    // path (matched by index); an aggregate operand / FILTER resolves under `Forbidden`, so this is
    // correctly inert there (its `a+b` is a per-row value, not the group key).
    if !matches!(e, Expr::Column(_) | Expr::QualifiedColumn { .. })
        && let Some((slot, ty)) = match_group_expr(agg, e)
    {
        return Ok((RExpr::Column(slot), ty));
    }
    match e {
        // A `ROW(...)` constructor (spec/design/composite.md §1): resolve each field with no type
        // context (its natural type), producing an ANONYMOUS composite (`name = None`, fields named
        // `f1, f2, …` per PG). Storing it into a named composite column matches structurally
        // (assignability at the store site coerces each field to the target's declared type).
        Expr::Row(items) => {
            let mut nodes = Vec::with_capacity(items.len());
            let mut fields = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                let (node, ty) = resolve(scope, it, None, agg, params)?;
                nodes.push(node);
                fields.push((format!("f{}", i + 1), ty));
            }
            Ok((
                RExpr::Row(nodes),
                ResolvedType::Composite(Box::new(CompositeRType { name: None, fields })),
            ))
        }
        // An `ARRAY[…]` constructor (spec/design/array.md §1): resolve each element (natural type),
        // unify to a common element type, and build a `RExpr::Array`. A bare empty `ARRAY[]` has no
        // element type to infer — use `'{}'::T[]` instead (the cast supplies the element type).
        Expr::Array(items) => {
            if items.is_empty() {
                return Err(type_error(
                    "cannot determine the element type of an empty ARRAY[]; write '{}'::T[]"
                        .to_string(),
                ));
            }
            // An element-type hint (`ctx`) flows down to the elements so an array literal adapts
            // its untyped integer/decimal literals exactly as a scalar literal does — e.g. resolving
            // `ARRAY[7,8]` with an i32 context yields `i32[]`, not the default `i64[]` (the
            // polymorphic array functions pass the bound element type here, array-functions.md §2).
            // Almost every other caller passes `None`, so the default 1-D unification is unchanged.
            let mut nodes = Vec::with_capacity(items.len());
            let mut elem_types = Vec::with_capacity(items.len());
            for it in items {
                let (node, ty) = resolve(scope, it, ctx, agg, params)?;
                nodes.push(node);
                elem_types.push(ty);
            }
            // Unify the item types. If they are themselves arrays, this is a **nested** (multidim-
            // stacking) constructor and the result type is the SAME array type (dimension-agnostic,
            // spec/design/array.md §2/§4); otherwise it is a flat 1-D array of the unified element.
            let common = unify_array_element_types(&elem_types)?;
            let (nested, result_ty) = match common {
                t @ ResolvedType::Array(_) => (true, t),
                other => (false, ResolvedType::Array(Box::new(other))),
            };
            Ok((
                RExpr::Array {
                    elems: nodes,
                    nested,
                },
                result_ty,
            ))
        }
        Expr::Column(name) => {
            // Resolve against the scope CHAIN (§26). Existence first (42703/42702 take priority,
            // matching PostgreSQL); a Local match then obeys the grouping rule, an Outer
            // (correlated) match is a per-outer-row constant exempt from it (see helper).
            let r = scope.resolve_bare(name)?;
            resolve_column_ref(scope, agg, r, name)
        }
        Expr::QualifiedColumn { qualifier, name } => {
            // A bare `rel.col` resolves strictly against the FROM relations — `qualifier` MUST name
            // a relation (else 42P01), matching PostgreSQL. Composite field access on a column is
            // the **parens-required** `(col).field` form (spec/design/composite.md §1/§S4), an
            // `Expr::FieldAccess`, never this bare qualified-column path (PG raises 42P01 for the
            // unparenthesized `col.field` / `t.col.field` spellings).
            let r = scope.resolve_qualified(qualifier, name)?;
            resolve_column_ref(scope, agg, r, name)
        }
        // `(expr).field` — composite field selection (spec/design/composite.md §S4).
        Expr::FieldAccess { base, field } => {
            let (node, ty) = resolve(scope, base, None, agg, params)?;
            resolve_field_of(node, ty, field)
        }
        // `base[..][..]` — array subscript (spec/design/array.md §6). The base must be an array
        // (else 42804). Each subscript bound is an integer (PG int4) — a literal adapts; a
        // non-integer is 42804. If any spec is a slice the result is the array type (a sub-array);
        // otherwise it is the element type (a single element). OOB / NULL → NULL is an
        // evaluation-time rule, not a resolve error.
        Expr::Subscript { base, subscripts } => {
            let (base_node, base_ty) = resolve(scope, base, None, agg, params)?;
            let elem_ty = match &base_ty {
                ResolvedType::Array(elem) => (**elem).clone(),
                other => {
                    return Err(type_error(format!(
                        "cannot subscript a value of type {}, which is not an array",
                        other.type_name()
                    )));
                }
            };
            let is_slice = subscripts
                .iter()
                .any(|s| matches!(s, SubscriptSpec::Slice(..)));
            let mut rsubs = Vec::with_capacity(subscripts.len());
            for s in subscripts {
                match s {
                    SubscriptSpec::Index(e) => {
                        rsubs.push(RSubscript::Index(Box::new(resolve_subscript_int(
                            scope, e, agg, params,
                        )?)));
                    }
                    SubscriptSpec::Slice(lo, hi) => {
                        let lower = match lo {
                            Some(e) => {
                                Some(Box::new(resolve_subscript_int(scope, e, agg, params)?))
                            }
                            None => None,
                        };
                        let upper = match hi {
                            Some(e) => {
                                Some(Box::new(resolve_subscript_int(scope, e, agg, params)?))
                            }
                            None => None,
                        };
                        rsubs.push(RSubscript::Slice { lower, upper });
                    }
                }
            }
            // A slice yields a sub-array (the array type); all-index access yields an element.
            let result_ty = if is_slice { base_ty } else { elem_ty };
            Ok((
                RExpr::Subscript {
                    base: Box::new(base_node),
                    subscripts: rsubs,
                    is_slice,
                },
                result_ty,
            ))
        }
        // `(expr).*` — whole-row expansion is a projection-list construct only; in a scalar
        // expression position it is unsupported (PG rejects row expansion here — 0A000).
        Expr::FieldStar { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "row expansion (.*) is not supported in this context",
        )),
        // `t.*` is likewise projection-list only — resolve_projections expands it before ever
        // calling resolve(); reaching here means it appeared in a scalar position (`WHERE t.*`,
        // `t.* + 1`), which is a syntax error (PG rejects a bare `t.*` outside the select list).
        Expr::QualifiedStar { .. } => Err(EngineError::new(
            SqlState::SyntaxError,
            "t.* is only allowed in a select list",
        )),
        Expr::Param(n1) => {
            // A bind parameter is an adaptable operand (like an integer/string literal): it
            // takes its type from `ctx` — the sibling operand, target column, or CAST target.
            // Record the inferred type (None = no context here; `finalize` 42P18s a parameter
            // that never gets one). spec/design/api.md §5.
            let idx0 = (*n1 as usize) - 1;
            params.note(idx0, ctx)?;
            let rty = match ctx {
                Some(t) => resolved_type_of(t),
                None => ResolvedType::Null,
            };
            Ok((RExpr::Param(idx0), rty))
        }
        Expr::FuncCall {
            name,
            args,
            arg_names,
            star,
            distinct,
            filter,
            variadic,
            over,
            over_name: _, // desugared to `over` before resolution (window.md §5)
            within_group,
        } => {
            // A hypothetical-set aggregate (rank/dense_rank/percent_rank/cume_dist — aggregates.md
            // §19) is one of these window-function names used WITH a WITHIN GROUP clause; that clause
            // routes it here instead of the window path. OVER + WITHIN GROUP together is 0A000.
            if is_hypothetical_set_name(name)
                && let Some(keys) = within_group.as_deref()
            {
                if over.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "OVER is not supported for hypothetical-set aggregate {}",
                            name.to_ascii_lowercase()
                        ),
                    ));
                }
                return resolve_hypothetical_set_aggregate(
                    scope,
                    name,
                    args,
                    keys,
                    *distinct,
                    filter.as_deref(),
                    agg,
                    params,
                );
            }
            // An ordered-set aggregate (mode/percentile_cont/percentile_disc — aggregates.md §13)
            // carries WITHIN GROUP and is resolved by its own path. OVER on one is 0A000 (PG itself
            // does not support an ordered-set aggregate as a window function); WITHOUT a WITHIN GROUP
            // it is 42883 (PG: "function mode() does not exist").
            if is_ordered_set_aggregate_name(name) {
                if over.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "OVER is not supported for ordered-set aggregate {}",
                            name.to_ascii_lowercase()
                        ),
                    ));
                }
                let Some(keys) = within_group.as_deref() else {
                    return Err(no_agg_overload(&name.to_ascii_lowercase()));
                };
                return resolve_ordered_set_aggregate(
                    scope,
                    name,
                    args,
                    keys,
                    *distinct,
                    filter.as_deref(),
                    agg,
                    params,
                );
            }
            // WITHIN GROUP on a non-ordered-set function (an ordinary aggregate or a scalar function)
            // is 42883 — PG models it as a missing overload (`sum(numeric, numeric) does not exist`).
            if within_group.is_some() {
                return Err(no_agg_overload(&name.to_ascii_lowercase()));
            }
            // A trailing OVER makes this a window-function call (spec/design/window.md §5.1).
            if let Some(wd) = over {
                // GROUPING is not a window function — `GROUPING(a) OVER ()` is a syntax error in
                // PostgreSQL (42601); match it rather than treating GROUPING as an unknown window fn.
                if name.eq_ignore_ascii_case("grouping") {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        "OVER is not supported for GROUPING",
                    ));
                }
                // DISTINCT is not implemented for window functions (PG 0A000 — aggregates.md §5):
                // a window aggregate folds over a frame, where per-frame de-duplication is undefined.
                if *distinct {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "DISTINCT is not implemented for window functions",
                    ));
                }
                // FILTER over a window function (aggregates.md §20). A window AGGREGATE folds only the
                // frame rows for which the filter is TRUE; a pure (non-aggregate) window function with
                // FILTER is PG's own 0A000 ("FILTER is not implemented for non-aggregate window
                // functions"). The filter is threaded into the WindowSpec and applied in the window stage.
                if filter.is_some() && !is_aggregate_name(name) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "FILTER is not implemented for non-aggregate window functions",
                    ));
                }
                return resolve_window_call(
                    scope,
                    name,
                    args,
                    *star,
                    wd,
                    filter.as_deref(),
                    agg,
                    params,
                );
            }
            // A window-only function (row_number/…) used WITHOUT OVER is 42809 (PG's
            // wrong_object_type, not the windowing_error 42P20 it uses for a window in WHERE —
            // window.md §7, oracle-verified).
            if is_window_only_name(name) {
                return Err(EngineError::new(
                    SqlState::WrongObjectType,
                    format!(
                        "window function {} requires an OVER clause",
                        name.to_ascii_lowercase()
                    ),
                ));
            }
            let names = arg_names.as_deref().map(Vec::as_slice);
            resolve_func_call(
                scope,
                name,
                args,
                names,
                *star,
                *distinct,
                filter.as_deref(),
                *variadic,
                agg,
                params,
            )
        }
        Expr::Literal(Literal::Null) => Ok((RExpr::ConstNull, ResolvedType::Null)),
        Expr::Literal(Literal::Bool(b)) => Ok((RExpr::ConstBool(*b), ResolvedType::Bool)),
        Expr::Literal(Literal::Int(n)) => {
            // An integer literal ADAPTS to a float context — decimal/int literal → float at the
            // context width (nearest, round-ties-to-even — spec/design/float.md §4). This is
            // literal adaptation, not an implicit cross-family cast (a *value* never silently
            // becomes a float). Otherwise it adapts only to an integer context, defaulting to
            // i64; a non-numeric context defers the family-mismatch check to the surroundings.
            if let Some(t) = ctx.filter(|t| t.is_float()) {
                return Ok((int_to_const_float(*n, t), ResolvedType::Float(t)));
            }
            let ty = match ctx {
                Some(t) if t.is_integer() => t,
                _ => ScalarType::Int64,
            };
            if !ty.in_range(*n) {
                return Err(overflow(ty));
            }
            Ok((RExpr::ConstInt(*n), ResolvedType::Int(ty)))
        }
        Expr::Literal(Literal::Text(s)) => {
            // A string literal is text by default (collation `C`). It adapts to a BYTEA context
            // (decode the hex input, 22P02), a UUID context (PG-flexible uuid input, 22P02 —
            // types.md §6/§13/§14), or a TIMESTAMP/TIMESTAMPTZ context (parse the datetime,
            // 22007/22008 — spec/design/timestamp.md). Any other context keeps it text.
            match ctx {
                Some(t) if t.is_bytea() => Ok((
                    RExpr::ConstBytea(decode_bytea_literal(s)?),
                    ResolvedType::Bytea,
                )),
                Some(t) if t.is_uuid() => Ok((
                    RExpr::ConstUuid(decode_uuid_literal(s)?),
                    ResolvedType::Uuid,
                )),
                Some(t) if t.is_timestamp() => Ok((
                    RExpr::ConstTimestamp(parse_timestamp(s)?),
                    ResolvedType::Timestamp,
                )),
                Some(t) if t.is_timestamptz() => Ok((
                    RExpr::ConstTimestamptz(parse_timestamptz(s)?),
                    ResolvedType::Timestamptz,
                )),
                // A string adapts to a DATE context (parse the ISO date, dropping any time/offset;
                // 22007/22008 — spec/design/date.md §2), exactly like timestamp adaptation. A
                // clock-relative special ('today'/'now'/…) becomes the STABLE DateClock node
                // instead of a constant (date.md §6).
                Some(t) if t.is_date() => {
                    if let Some((node, rt)) = date_clock_literal(s, params) {
                        return Ok((node, rt));
                    }
                    Ok((RExpr::ConstDate(parse_date(s)?), ResolvedType::Date))
                }
                // A string adapts to an INTERVAL context (parse the "unit + time" subset,
                // 22007/22008 — spec/design/interval.md), exactly like timestamp adaptation.
                Some(t) if t.is_interval() => Ok((
                    RExpr::ConstInterval(parse_interval(s)?),
                    ResolvedType::Interval,
                )),
                // A string literal adapts to a json/jsonb context (the sibling of a jsonb column /
                // a jsonb cast), so `jsonbcol = '{"a":1}'` compares jsonb × jsonb; malformed → 22P02
                // (spec/design/json.md §2/§4). json validates + stores verbatim; jsonb canonicalizes.
                Some(t) if t.is_json() => {
                    json::validate_json(s)?;
                    Ok((RExpr::ConstJson(s.clone()), ResolvedType::Json))
                }
                Some(t) if t.is_jsonb() => Ok((
                    RExpr::ConstJsonb(Box::new(json::jsonb_in(s)?)),
                    ResolvedType::Jsonb,
                )),
                // A string literal adapts to a jsonpath context (a jsonpath function argument) — it is
                // compiled to a path at resolve (jsonpath.md §1); malformed → 42601.
                Some(ScalarType::JsonPath) => Ok((
                    RExpr::ConstJsonPath(crate::jsonpath::JsonPath::compile(s)?.render()),
                    ResolvedType::JsonPath,
                )),
                _ => Ok((RExpr::ConstText(s.clone()), ResolvedType::Text)),
            }
        }
        Expr::Literal(Literal::Decimal(d)) => {
            // A decimal literal ADAPTS to a float context — decimal → float at the context width
            // (nearest binary value, round-ties-to-even — spec/design/float.md §4). Otherwise it
            // stays decimal (it does not adapt to other contexts, like text). Cap-check the
            // decimal value here (an over-long coefficient/scale traps 22003 at resolve —
            // spec/design/decimal.md §6).
            if let Some(t) = ctx.filter(|t| t.is_float()) {
                return Ok(match decimal_to_float(d, t)? {
                    Value::Float32(f) => (RExpr::ConstFloat32(f), ResolvedType::Float(t)),
                    Value::Float64(f) => (RExpr::ConstFloat64(f), ResolvedType::Float(t)),
                    _ => unreachable!("decimal_to_float returns a float value"),
                });
            }
            let d = d.clone().check_cap()?;
            Ok((RExpr::ConstDecimal(d), ResolvedType::Decimal))
        }
        // A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's
        // `type 'string'`, equal to `CAST('string' AS type)` over a string-literal operand. Resolve
        // the type by name (unknown → 42704) and coerce the string to it at resolve, independent of
        // any context. No typmod rides on the literal (the parser's one-token lookahead admits none).
        Expr::TypedLiteral { type_name, text } => {
            // A composite type name (`addr '(Main,90210)'`) coerces the string via `record_in`
            // (spec/design/composite.md §8) — the same primitive as `'(…)'::addr`.
            if let Some(ct) = scope.catalog.composite_type(type_name) {
                return coerce_string_to_composite(text, ct, scope.catalog);
            }
            // A range type name (`i32range '[1,5)'`, `int4range '…'`) coerces the string via
            // `range_in` against the element type (spec/design/ranges.md §5) — the same primitive
            // as `'[1,5)'::i32range`.
            if let Some(desc) = crate::range::range_by_name(type_name) {
                return coerce_string_to_range_expr(text, desc);
            }
            let (target, _, _) = resolve_type_and_typmod(type_name, &None)?;
            // DATE 'today' / DATE 'now' / … — the clock-relative specials become the STABLE
            // DateClock node, exactly like the ctx-adaptation form (date.md §6).
            if target.is_date() {
                if let Some((node, rt)) = date_clock_literal(text, params) {
                    return Ok((node, rt));
                }
            }
            coerce_string_literal(text, target, None, None)
        }
        // A subquery in expression position (spec/design/grammar.md §26): PLANNED ONCE against the
        // scope chain here, so its column-count / type errors fire even over an empty outer.
        // `plan_subquery` rejects a non-SELECT context and a `$N` inside (both 0A000). The fold
        // pass folds an uncorrelated one to a constant; a correlated one (an OuterColumn in its
        // plan) is re-executed per outer row by the evaluator.
        Expr::ScalarSubquery(inner) => {
            let plan = plan_subquery(scope, inner, params)?;
            if plan.column_types().len() != 1 {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "subquery must return only one column",
                ));
            }
            let out_type = plan.column_types()[0].clone();
            Ok((
                RExpr::Subquery {
                    plan: Box::new(plan),
                    kind: SubqueryKind::Scalar,
                    lhs: None,
                    negated: false,
                },
                out_type,
            ))
        }
        Expr::Exists(inner) => {
            // EXISTS ignores the select list entirely; the result is boolean, never NULL. A NOT
            // EXISTS parses as the unary `NOT` wrapping this, so `negated` here is always false.
            let plan = plan_subquery(scope, inner, params)?;
            Ok((
                RExpr::Subquery {
                    plan: Box::new(plan),
                    kind: SubqueryKind::Exists,
                    lhs: None,
                    negated: false,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::InSubquery {
            lhs,
            query,
            negated,
        } => {
            // The LHS is an OUTER expression (resolved in the current scope / agg context); the
            // subquery yields the single membership column. The test is `lhs = element`, so the
            // pair must be comparable (42804), exactly like a literal IN.
            let (rlhs, lt) = resolve(scope, lhs, None, agg, params)?;
            let plan = plan_subquery(scope, query, params)?;
            if plan.column_types().len() != 1 {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "subquery has too many columns",
                ));
            }
            classify_comparable(&lt, &plan.column_types()[0])?;
            Ok((
                RExpr::Subquery {
                    plan: Box::new(plan),
                    kind: SubqueryKind::In,
                    lhs: Some(Box::new(rlhs)),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        // `expr COLLATE "name"` (spec/design/collation.md §1) — a postfix collation operator. Resolve
        // the inner expression, require a collatable (text) type (42804, PG-matching), and validate
        // the named collation exists ("C" or loaded, else 42704). The node is a runtime PASSTHROUGH:
        // a collation only changes the ORDERING comparisons / ORDER BY, derived from the AST at those
        // sites (`explicit_collation` / `OrderKey.collation`), so resolving returns the inner resolved
        // expr + type unchanged. The hint flows through (COLLATE never changes the type).
        Expr::Collate { inner, collation } => {
            let (rinner, ty) = resolve(scope, inner, ctx, agg, params)?;
            if !matches!(ty, ResolvedType::Text | ResolvedType::Null) {
                return Err(type_error(format!(
                    "collations are not supported by type {}",
                    ty.type_name()
                )));
            }
            // Validate the name resolves (surfaces 42704 for an unknown collation); the value is
            // recovered at the comparison/ORDER BY site, so it is discarded here.
            resolve_collation_name(scope.catalog, collation)?;
            Ok((rinner, ty))
        }
        // `EXTRACT(field FROM source)` (timezones.md §9.2, grammar.md §50). The field is SYNTACTIC and
        // validated at RESOLVE (not per row): an unsupported field for the source type is `0A000`, an
        // unrecognized field is `22023` — surfaced by probing the kernel with a zero value of the
        // source's family. The source must be a datetime type (else `42883`); the result is `numeric`.
        Expr::Extract { field, source } => {
            use crate::datetime_fn::{ExtractSrc, extract_field};
            let (src_r, src_t) = resolve(scope, source, None, agg, params)?;
            // A NULL source has no resolvable family; the value propagates to NULL at eval (the field
            // is not validated — a documented narrow edge vs. PG, which still errors on a bad field).
            if !matches!(src_t, ResolvedType::Null) {
                let probe = match src_t {
                    ResolvedType::Timestamp => ExtractSrc::Timestamp(0),
                    ResolvedType::Timestamptz => ExtractSrc::Timestamptz {
                        instant: 0,
                        local: 0,
                        offset_secs: 0,
                    },
                    ResolvedType::Date => ExtractSrc::Date(0),
                    ResolvedType::Interval => ExtractSrc::Interval(crate::interval::Interval {
                        months: 0,
                        days: 0,
                        micros: 0,
                    }),
                    _ => {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            format!(
                                "function extract(text, {}) does not exist",
                                src_t.type_name()
                            ),
                        ));
                    }
                };
                // Validate field-for-type (0A000 / 22023); the value is discarded.
                extract_field(field, probe)?;
            }
            Ok((
                RExpr::Extract {
                    field: field.clone(),
                    value: Box::new(src_r),
                },
                ResolvedType::Decimal,
            ))
        }
        Expr::Cast {
            inner,
            type_name,
            type_mod,
        } => {
            // An array cast target `…::T[]` (spec/design/array.md §7). v1 supports only the
            // string-literal form `'{…}'::T[]` and a bare NULL; every other array cast (runtime
            // text→array, array→text, element-wise array→array) is a documented 0A000 narrowing.
            // The element is a scalar or a previously-defined composite (array-of-composite, §12 AC1).
            if let Some(base) = type_name.strip_suffix("[]") {
                if type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier on an array type is not supported yet".to_string(),
                    ));
                }
                let (elem_col, elem_rt): (ColType, ResolvedType) = match ScalarType::from_name(base)
                {
                    Some(s) => (ColType::Scalar(s), resolved_type_of(s)),
                    None => match scope.catalog.composite_type(base) {
                        Some(ct) => {
                            let cty = Type::Composite(crate::types::CompositeRef {
                                name: ct.name.clone(),
                            });
                            let col = resolve_col_type(&cty, &scope.catalog.read_snap().types);
                            let rt = resolved_type_of_col(&cty, scope.catalog);
                            (col, rt)
                        }
                        None => {
                            return Err(EngineError::new(
                                SqlState::UndefinedObject,
                                format!("type does not exist: {base}"),
                            ));
                        }
                    },
                };
                if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                    let val = coerce_string_to_array(s, &elem_col)?;
                    return Ok((value_to_rexpr(&val), ResolvedType::Array(Box::new(elem_rt))));
                }
                if let Expr::Literal(Literal::Null) = inner.as_ref() {
                    return Ok((RExpr::ConstNull, ResolvedType::Array(Box::new(elem_rt))));
                }
                // A bind parameter into an array stays the documented container-param narrowing
                // (0A000), like INSERT's `$N`-into-a-container handling (spec/design/array.md §4).
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting a parameter to an array type is not supported yet".to_string(),
                    ));
                }
                // A runtime (non-literal) operand: the two follow-on array-producing casts
                // (spec/design/array.md §7). A `text` expression coerces per row via `array_in`
                // (runtime text→T[]); an array of the SAME element type is the identity (no node);
                // an array of a DIFFERENT element type is an element-wise array→array cast (each
                // element through the scalar cast, when the element pair is castable); a non-literal
                // NULL adapts. Any other source is a 42804 datatype mismatch.
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                let result_rt = ResolvedType::Array(Box::new(elem_rt.clone()));
                return match ity {
                    ResolvedType::Null => Ok((rinner, result_rt)),
                    ResolvedType::Text => Ok((
                        RExpr::ArrayCast {
                            inner: Box::new(rinner),
                            to_elem: Some(elem_col),
                        },
                        result_rt,
                    )),
                    ResolvedType::Array(ref src_elem) if **src_elem == elem_rt => {
                        Ok((rinner, result_rt)) // identity cast — same element type
                    }
                    ResolvedType::Array(ref src_elem) => {
                        match (resolved_to_scalar(src_elem), &elem_col) {
                            (Some(src_s), ColType::Scalar(tgt_s))
                                if scalar_pair_castable(src_s, *tgt_s) =>
                            {
                                Ok((
                                    RExpr::ArrayCast {
                                        inner: Box::new(rinner),
                                        to_elem: Some(elem_col),
                                    },
                                    result_rt,
                                ))
                            }
                            // A composite element on either side is the composite cast surface
                            // (0A000 — composite casts are deferred, composite.md §8/§12).
                            (None, _) | (_, ColType::Composite { .. }) => Err(EngineError::new(
                                SqlState::FeatureNotSupported,
                                "casting between composite-element arrays is not supported yet"
                                    .to_string(),
                            )),
                            // Both elements are scalars but no cast exists between them — forbidden
                            // (42804, jed's strict-matrix convention; PG reports 42846).
                            _ => Err(type_error(format!(
                                "cannot cast {} to {base}[]",
                                ity.type_name()
                            ))),
                        }
                    }
                    _ => Err(type_error(format!(
                        "cannot cast {} to {base}[]",
                        ity.type_name()
                    ))),
                };
            }
            // A range cast target (`'[1,5)'::i32range`, `…::int4range`). Like array, v1 supports the
            // string-literal form and a bare NULL; every other range cast (runtime text→range,
            // range→text) is a documented 0A000 narrowing (spec/design/ranges.md §1/§5).
            if let Some(desc) = crate::range::range_by_name(type_name) {
                if type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier on a range type is not supported".to_string(),
                    ));
                }
                let elem_rt = resolved_type_of(crate::range::element_scalar(desc));
                if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                    return coerce_string_to_range_expr(s, desc);
                }
                if let Expr::Literal(Literal::Null) = inner.as_ref() {
                    return Ok((RExpr::ConstNull, ResolvedType::Range(Box::new(elem_rt))));
                }
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to a range type is only supported from a string literal this slice"
                        .to_string(),
                ));
            }
            // A composite cast target (`'(…)'::addr`) — a CREATE TYPE name, not a built-in scalar
            // (spec/design/composite.md §8). A STRING LITERAL operand coerces via `record_in` (the
            // `'(…)'::addr` headline); a bare NULL adapts to the composite; a same-named composite
            // operand is the identity. Every other operand (a runtime text expression, an anonymous
            // `ROW(…)`) is a documented `0A000` narrowing this slice — relaxable. A type modifier on
            // a composite is meaningless (`0A000`).
            if let Some(ct) = scope.catalog.composite_type(type_name) {
                if type_mod.is_some() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "a type modifier is not supported on a composite type",
                    ));
                }
                if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                    return coerce_string_to_composite(s, ct, scope.catalog);
                }
                let ct_name = ct.name.clone();
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                return match &ity {
                    ResolvedType::Null => Ok((
                        rinner,
                        resolved_type_of_col(
                            &Type::Composite(crate::types::CompositeRef { name: ct_name }),
                            scope.catalog,
                        ),
                    )),
                    // An identical named composite is the identity cast.
                    ResolvedType::Composite(c) if c.name.as_deref() == Some(ct_name.as_str()) => {
                        Ok((rinner, ity))
                    }
                    _ => Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting to a composite type is only supported from a string literal",
                    )),
                };
            }
            let (target, typmod, varchar_len) = resolve_type_and_typmod(type_name, type_mod)?;
            // A string LITERAL operand is coerced to the target at resolve — `CAST('42' AS int)`,
            // the same primitive as the `type 'string'` typed literal (grammar.md §36, types.md §5).
            // This is the ONLY text→T cast admitted ahead of the general cast slice; a non-literal
            // text operand still falls through to the deferred 0A000 below. A `varchar(n)` target
            // truncates the literal to n code points (types.md §15).
            if let Expr::Literal(Literal::Text(s)) = inner.as_ref() {
                // 'today'::date / CAST('now' AS date) — the clock-relative specials become the
                // STABLE DateClock node, exactly like the ctx-adaptation form (date.md §6).
                if target.is_date() {
                    if let Some((node, rt)) = date_clock_literal(s, params) {
                        return Ok((node, rt));
                    }
                }
                return coerce_string_literal(s, target, typmod, varchar_len);
            }
            // Cross-family datetime casts (timezones.md §9.3): a `timestamp`/`timestamptz`/`date`
            // TARGET from another datetime family. A same-family cast is the identity; a cross-family
            // cast becomes a `DateConvert` node (the zone-crossing ones read the session zone at eval);
            // any non-datetime source is the deferred `0A000`. `text`-literal operands and bind params
            // are handled above / just below. A `NULL` operand adapts to the target.
            if target.is_timestamp() || target.is_timestamptz() || target.is_date() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    // `$1::timestamp` declares the parameter as the target type (the cast-target
                    // parameter-typing case, api.md §5), exactly like the generic path below.
                    let (rinner, _) = resolve(scope, inner, Some(target), agg, params)?;
                    return Ok((rinner, resolved_type_of(target)));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                let to_rt = resolved_type_of(target);
                return match ity {
                    ResolvedType::Null => Ok((rinner, to_rt)),
                    ResolvedType::Timestamp if target.is_timestamp() => Ok((rinner, ity)),
                    ResolvedType::Timestamptz if target.is_timestamptz() => Ok((rinner, ity)),
                    ResolvedType::Date if target.is_date() => Ok((rinner, ity)),
                    ResolvedType::Timestamp | ResolvedType::Timestamptz | ResolvedType::Date => {
                        Ok((
                            RExpr::DateConvert {
                                inner: Box::new(rinner),
                                to: target,
                            },
                            to_rt,
                        ))
                    }
                    ResolvedType::Text if target.is_date() => {
                        // The runtime text → date cast (date.md §6): a NON-literal text source (a
                        // string LITERAL operand was folded by `coerce_string_literal` above) parses
                        // per row via the same `parse_date` the literal uses (22007/22008 per row).
                        // STABLE, not immutable — the input grammar admits the clock-relative
                        // specials — so it flags the plan non-immutable (42P17 in an index
                        // expression, as in PG). text → timestamp/timestamptz stays deferred.
                        params.nonimmutable = true;
                        Ok((
                            RExpr::DateConvert {
                                inner: Box::new(rinner),
                                to: target,
                            },
                            to_rt,
                        ))
                    }
                    _ => Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "cannot cast {} to {}",
                            ity.type_name(),
                            target.canonical_name()
                        ),
                    )),
                };
            }
            // The JSON cast matrix (spec/design/json.md §6.1): casting TO json/jsonb from a runtime
            // text/json/jsonb expression (a string LITERAL operand was already coerced above by
            // `coerce_string_literal`). text → json validates + stores verbatim; text → jsonb parses
            // + canonicalizes; json → jsonb re-parses + canonicalizes; jsonb → json renders the
            // canonical text; same-type is the identity. Any other source is a 42846 cast error.
            if target.is_json() || target.is_jsonb() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    let (rinner, _) = resolve(scope, inner, Some(target), agg, params)?;
                    return Ok((rinner, resolved_type_of(target)));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                let to_rt = resolved_type_of(target);
                return match ity {
                    ResolvedType::Null => Ok((rinner, to_rt)),
                    ResolvedType::Text | ResolvedType::Json | ResolvedType::Jsonb => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod: None,
                            varchar_len: None,
                        },
                        to_rt,
                    )),
                    _ => Err(type_error(format!(
                        "cannot cast type {} to {}",
                        ity.type_name(),
                        target.canonical_name()
                    ))),
                };
            }
            // Text casts are deferred (not in the cast matrix — spec/design/types.md §5/§11), EXCEPT
            // json/jsonb → text (the JSON cast matrix, json.md §6.1): json → text is the identity on
            // the verbatim bytes, jsonb → text renders the canonical form. A NULL adapts. Every other
            // text cast target is still a 0A000 this slice — including `$1::text` (declaring a bind
            // param as text via a cast stays deferred, the params.rs contract — guarded first so it
            // does not resolve to an untyped-NULL text node and trip 42P18).
            if target.is_text() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting to text is not supported yet",
                    ));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                return match ity {
                    // text → text: the identity, UNLESS a `varchar(n)` length is present — then it
                    // becomes a real Cast node that silently truncates to n code points at eval
                    // (types.md §15). A NULL adapts (NULL → NULL, no truncation needed).
                    ResolvedType::Null => Ok((rinner, ResolvedType::Text)),
                    ResolvedType::Text => Ok((
                        match varchar_len {
                            Some(_) => RExpr::Cast {
                                inner: Box::new(rinner),
                                target,
                                typmod: None,
                                varchar_len,
                            },
                            None => rinner,
                        },
                        ResolvedType::Text,
                    )),
                    // json/jsonb → text (the JSON cast matrix) and uuid → text (the uuid cast slice,
                    // casts.toml/types.md §14: the canonical lowercase 8-4-4-4-12 form). Explicit —
                    // stricter than PG's assignment-cast-to-text (a documented divergence). A
                    // `varchar(n)` length truncates the rendered text (types.md §15).
                    ResolvedType::Json | ResolvedType::Jsonb | ResolvedType::Uuid => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod: None,
                            varchar_len,
                        },
                        ResolvedType::Text,
                    )),
                    // array → text (spec/design/array.md §7): `array_out` renders `{…}` per row.
                    // Explicit only — like uuid/json → text, stricter than PG's assignment cast (so
                    // `INSERT INTO text_col VALUES (arr)` stays 42804). Handled by `ArrayCast`.
                    ResolvedType::Array(_) => Ok((
                        RExpr::ArrayCast {
                            inner: Box::new(rinner),
                            to_elem: None,
                        },
                        ResolvedType::Text,
                    )),
                    _ => Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting to text is not supported yet",
                    )),
                };
            }
            // A boolean target (`CAST(x AS boolean)`, `x::boolean`) is the boolean cast slice
            // (spec/types/casts.toml, types.md §9). It needs the inner type to decide (only an i32
            // / NULL / bool source is castable), so it is handled AFTER the inner is resolved, below
            // — not guarded here.
            // A bytea TARGET: the uuid cast slice admits uuid → bytea (the 16 raw bytes — a jed cast
            // PG lacks; casts.toml, types.md §14). A string LITERAL was coerced above; a NULL adapts;
            // a bytea operand is the identity. text → bytea and every other bytea cast stay deferred
            // (0A000 — the bytea cast slice's own follow-on, types.md §13).
            if target.is_bytea() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    let (rinner, _) = resolve(scope, inner, Some(ScalarType::Bytea), agg, params)?;
                    return Ok((rinner, ResolvedType::Bytea));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                return match ity {
                    ResolvedType::Null | ResolvedType::Bytea => Ok((rinner, ResolvedType::Bytea)),
                    ResolvedType::Uuid => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod: None,
                            varchar_len: None,
                        },
                        ResolvedType::Bytea,
                    )),
                    _ => Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting to bytea is not supported yet",
                    )),
                };
            }
            // The uuid cast slice (spec/types/casts.toml, types.md §14): a uuid TARGET from a runtime
            // text or bytea expression. text → uuid runs uuid_in at eval (22P02 on malformed);
            // bytea → uuid takes the 16 raw bytes (22P02 on a length ≠ 16) — a jed cast PG lacks. A
            // string LITERAL operand was already coerced above (the §6 adaptation); `$1::uuid`
            // declares the param as uuid; a NULL adapts; a uuid operand is the identity.
            if target.is_uuid() {
                if matches!(inner.as_ref(), Expr::Param(_)) {
                    let (rinner, _) = resolve(scope, inner, Some(ScalarType::Uuid), agg, params)?;
                    return Ok((rinner, ResolvedType::Uuid));
                }
                let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
                return match ity {
                    ResolvedType::Null | ResolvedType::Uuid => Ok((rinner, ResolvedType::Uuid)),
                    ResolvedType::Text | ResolvedType::Bytea => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod: None,
                            varchar_len: None,
                        },
                        ResolvedType::Uuid,
                    )),
                    _ => Err(type_error(format!(
                        "cannot cast {} to uuid",
                        ity.type_name()
                    ))),
                };
            }
            // The timestamp/timestamptz/date cross-family cast matrix is handled above (the
            // `DateConvert` block — timezones.md §9.3). `text`↔datetime casts (a string lands in a
            // datetime column by literal adaptation, not a CAST) stay deferred and fall through to the
            // generic logic below (which rejects a non-datetime source to a datetime target 0A000).
            // interval casts are deferred (spec/design/interval.md): casting TO interval is 0A000
            // (a string lands in an interval column by literal adaptation / the INTERVAL '...'
            // keyword literal, not a CAST).
            if target.is_interval() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to an interval type is not supported yet",
                ));
            }
            // A bind-parameter operand takes the cast TARGET as its inferred type — `$1::int`
            // (and `CAST($1 AS int)`) declares `$1` as int, the cast-target parameter-typing case
            // (spec/design/api.md §5, grammar.md §37). Every other operand resolves with NO literal
            // context — its value is range-checked / coerced against `target` at eval — so changing
            // the context only for a parameter leaves all existing CAST behavior untouched.
            let inner_ctx = if matches!(inner.as_ref(), Expr::Param(_)) {
                Some(target)
            } else if target.is_bool() {
                // A boolean TARGET accepts only an i32 source (the boolean cast slice). An untyped
                // integer literal operand therefore adapts to i32 — `CAST(5 AS boolean)` / `5::boolean`
                // — matching PG (a bare `5` is int4, then int4→bool). Without this the literal would
                // default to i64 and the i64→boolean pair is forbidden. A column/expression operand
                // ignores this literal context and keeps its own type (an i64 column → 42804). A
                // literal beyond i32 range then traps 22003 (PG says 42846 — a documented divergence).
                Some(ScalarType::Int32)
            } else {
                None
            };
            let (rinner, ity) = resolve(scope, inner, inner_ctx, agg, params)?;
            // The boolean cast slice (spec/types/casts.toml, types.md §9): PG ties boolean↔integer to
            // i32 ONLY and makes both directions explicit. A boolean TARGET takes an i32 / NULL / bool
            // source (the eval maps 0→false, nonzero→true); a boolean SOURCE produces an i32 (true→1,
            // false→0). Both are handled here, ahead of the generic numeric cast logic below — the
            // generic `result_ty` assumes an int/decimal/float target, so a boolean target must not
            // fall through. A bool⇄i16 / bool⇄i64 pair is a forbidden 42804 (jed's datatype-mismatch
            // convention; PG reports 42846 — a documented divergence, casts.toml).
            if target.is_bool() {
                return match ity {
                    // A runtime `text` source is the runtime-text-cast slice (grammar.md §36): the
                    // eval parses the per-row string via the same `parse_bool_literal` (PG boolin)
                    // the `'t'::boolean` literal uses. A string LITERAL operand was already coerced
                    // above, so a `Text` here is non-literal (a column / expression).
                    ResolvedType::Int(ScalarType::Int32)
                    | ResolvedType::Bool
                    | ResolvedType::Text
                    | ResolvedType::Null => Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod,
                            varchar_len: None,
                        },
                        ResolvedType::Bool,
                    )),
                    _ => Err(type_error(format!(
                        "cannot cast {} to boolean",
                        ity.type_name()
                    ))),
                };
            }
            if matches!(ity, ResolvedType::Bool) {
                if target == ScalarType::Int32 {
                    return Ok((
                        RExpr::Cast {
                            inner: Box::new(rinner),
                            target,
                            typmod,
                            varchar_len: None,
                        },
                        ResolvedType::Int(ScalarType::Int32),
                    ));
                }
                return Err(type_error(format!(
                    "cannot cast boolean to {}",
                    target.canonical_name()
                )));
            }
            match ity {
                // int→int (range check), int→decimal (widen), decimal→int (explicit, round),
                // decimal→decimal (re-scale), and NULL are all castable. Floats add int↔float,
                // decimal↔float, and float↔float (spec/design/float.md §6 — all explicit; the
                // eval does the rounding/range-check), so a Float inner is castable too.
                ResolvedType::Int(_)
                | ResolvedType::Decimal
                | ResolvedType::Float(_)
                | ResolvedType::Null => {}
                // A boolean source is handled above (the boolean cast slice) — unreachable here.
                ResolvedType::Bool => unreachable!("boolean cast operand handled above"),
                // A runtime `text` source to a numeric target is the runtime-text-cast slice
                // (grammar.md §36): the only targets reaching this generic path are int / decimal /
                // float (text / bytea / uuid / datetime / interval / bool / json targets all return
                // in their own blocks above), so a `Text` here casts to a number. The eval coerces
                // the per-row string via the same parse functions the literal form uses (22P02 /
                // 22003 per row). A string LITERAL operand was already folded above, so this `Text`
                // is non-literal (a column / expression). Fall through to the numeric `Cast` node.
                ResolvedType::Text => {}
                // Casting FROM bytea is likewise deferred (0A000).
                // Casting FROM bytea is likewise deferred (0A000).
                ResolvedType::Bytea => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from bytea is not supported yet",
                    ));
                }
                // Casting FROM uuid is likewise deferred (0A000).
                ResolvedType::Uuid => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from uuid is not supported yet",
                    ));
                }
                // Casting FROM a timestamp is likewise deferred (0A000).
                ResolvedType::Timestamp | ResolvedType::Timestamptz => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from a timestamp type is not supported yet",
                    ));
                }
                // Casting FROM an interval is likewise deferred (0A000).
                ResolvedType::Interval => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from an interval type is not supported yet",
                    ));
                }
                // Casting FROM a date is likewise deferred (0A000; date↔timestamp unblocks the
                // cross-family comparison — spec/design/date.md §4/§6).
                ResolvedType::Date => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from a date type is not supported yet",
                    ));
                }
                // Casting a composite (text↔composite) lands in a later slice (composite.md §8/§12).
                ResolvedType::Composite(_) => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting a composite value is not supported yet",
                    ));
                }
                // Casting FROM an array (array→text, element-wise array→array) is deferred
                // (spec/design/array.md §7/§12).
                ResolvedType::Array(_) => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting an array value is not supported yet",
                    ));
                }
                // Casting FROM a range (range→text, range→range) is deferred (ranges.md §5/§10);
                // a range cast TARGET is handled above (the string-literal form).
                ResolvedType::Range(_) => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting a range value is not supported yet",
                    ));
                }
                // Casting FROM json/jsonb (json↔jsonb, json[b]→text, text→json[b]) lands in J3
                // (spec/design/json.md §6); deferred this slice.
                ResolvedType::Json | ResolvedType::Jsonb | ResolvedType::JsonPath => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting a json value is not supported yet",
                    ));
                }
            }
            let result_ty = if target.is_decimal() {
                ResolvedType::Decimal
            } else if target.is_float() {
                ResolvedType::Float(target)
            } else {
                ResolvedType::Int(target)
            };
            Ok((
                RExpr::Cast {
                    inner: Box::new(rinner),
                    target,
                    typmod,
                    varchar_len: None,
                },
                result_ty,
            ))
        }
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => {
            let (rop, ty) = resolve(scope, operand, ctx, agg, params)?;
            let result = match ty {
                ResolvedType::Int(t) => t,
                ResolvedType::Decimal => ScalarType::Decimal,
                // -float flips the sign bit (no overflow; a NaN/Inf operand passes through —
                // spec/design/float.md §5). The result keeps the operand's width.
                ResolvedType::Float(t) => t,
                ResolvedType::Null => ScalarType::Int64, // -NULL = NULL
                ResolvedType::Interval => ScalarType::Interval, // -interval (interval.md §5)
                ResolvedType::Bool
                | ResolvedType::Text
                | ResolvedType::Bytea
                | ResolvedType::Uuid
                | ResolvedType::Timestamp
                | ResolvedType::Timestamptz
                | ResolvedType::Date
                | ResolvedType::Json
                | ResolvedType::Jsonb
                | ResolvedType::JsonPath
                | ResolvedType::Composite(_)
                | ResolvedType::Array(_)
                | ResolvedType::Range(_) => {
                    return Err(type_error("unary minus requires a numeric operand"));
                }
            };
            let rty = if result.is_decimal() {
                ResolvedType::Decimal
            } else if result.is_interval() {
                ResolvedType::Interval
            } else if result.is_float() {
                ResolvedType::Float(result)
            } else {
                ResolvedType::Int(result)
            };
            Ok((
                RExpr::Neg {
                    operand: Box::new(rop),
                    result,
                },
                rty,
            ))
        }
        Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } => {
            let (rop, ty) = resolve(scope, operand, None, agg, params)?;
            require_bool(&ty, "NOT requires a boolean operand")?;
            Ok((RExpr::Not(Box::new(rop)), ResolvedType::Bool))
        }
        Expr::IsNull { operand, negated } => {
            // IS [NOT] NULL accepts any operand type and always yields a definite boolean.
            let (rop, _ty) = resolve(scope, operand, None, agg, params)?;
            Ok((
                RExpr::IsNull {
                    operand: Box::new(rop),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::IsJson {
            operand,
            negated,
            kind,
            unique_keys,
        } => {
            // The operand must be a character string / json / jsonb (else 42804); a bare string
            // literal resolves as text. The predicate is always a definite boolean (NULL operand →
            // NULL at eval).
            let (rop, ty) = resolve(scope, operand, None, agg, params)?;
            match ty {
                ResolvedType::Text
                | ResolvedType::Json
                | ResolvedType::Jsonb
                | ResolvedType::JsonPath
                | ResolvedType::Null => {}
                _ => {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        format!("cannot use type {} in IS JSON predicate", ty.type_name()),
                    ));
                }
            }
            Ok((
                RExpr::IsJson {
                    operand: Box::new(rop),
                    negated: *negated,
                    kind: *kind,
                    unique_keys: *unique_keys,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::JsonCtor {
            operand,
            unique_keys,
        } => {
            // JSON(text) parses a character string to a `json` value (verbatim). The operand must be
            // text (a bare string literal stays text); a non-text operand → 42804.
            let (rop, ty) = resolve(scope, operand, Some(ScalarType::Text), agg, params)?;
            match ty {
                ResolvedType::Text | ResolvedType::Null => {}
                _ => {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        format!("cannot use type {} as JSON() input", ty.type_name()),
                    ));
                }
            }
            Ok((
                RExpr::JsonCtor {
                    operand: Box::new(rop),
                    unique_keys: *unique_keys,
                },
                ResolvedType::Json,
            ))
        }
        Expr::JsonExists {
            ctx,
            path,
            on_error,
        } => resolve_json_sql_fn(
            scope,
            JsonSqlKind::Exists,
            ctx,
            path,
            &None,
            JsonWrapper::Without,
            true,
            &None,
            on_error,
            agg,
            params,
        ),
        Expr::JsonValue {
            ctx,
            path,
            returning,
            on_empty,
            on_error,
        } => resolve_json_sql_fn(
            scope,
            JsonSqlKind::Value,
            ctx,
            path,
            returning,
            JsonWrapper::Without,
            true,
            on_empty,
            on_error,
            agg,
            params,
        ),
        Expr::JsonQuery {
            ctx,
            path,
            returning,
            wrapper,
            keep_quotes,
            on_empty,
            on_error,
        } => resolve_json_sql_fn(
            scope,
            JsonSqlKind::Query,
            ctx,
            path,
            returning,
            *wrapper,
            *keep_quotes,
            on_empty,
            on_error,
            agg,
            params,
        ),
        Expr::IsDistinctFrom { lhs, rhs, negated } => {
            // NULL-safe equality: the SAME operand contract as `=` — resolve the pair
            // (a literal adapts to its sibling; a text literal stays text), then require
            // the operands be comparable (both integer-ish or both text-ish; a mixed pair
            // is 42804). The result is always a definite boolean (functions.md §3).
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            classify_comparable(&lt, &rt)?;
            Ok((
                RExpr::Distinct {
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::Binary { op, lhs, rhs } => resolve_binary(scope, *op, lhs, rhs, agg, params),
        Expr::Quantified {
            op,
            all,
            lhs,
            array,
        } => resolve_quantified(scope, *op, *all, lhs, array, agg, params),
        Expr::QuantifiedSubquery {
            op,
            all,
            lhs,
            query,
        } => {
            // The subquery spelling of the quantifier (array-functions.md §11.6) — the IN-subquery
            // pattern, with the comparison + 3VL fold of the array form. Resolve the outer `lhs`,
            // plan the body, require ONE column (42601), and require comparability — reporting
            // operator-not-found (42883) the way the array quantifier does (§11.3), not the plain
            // 42804. No 21000 cardinality limit (any row count is a list).
            let (rlhs, lt) = resolve(scope, lhs, None, agg, params)?;
            let plan = plan_subquery(scope, query, params)?;
            if plan.column_types().len() != 1 {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "subquery has too many columns",
                ));
            }
            classify_comparable(&lt, &plan.column_types()[0]).map_err(|_| {
                EngineError::new(
                    SqlState::UndefinedFunction,
                    format!(
                        "operator does not exist: {} {} {}",
                        lt.type_name(),
                        binary_op_symbol(*op),
                        plan.column_types()[0].type_name()
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
                _ => unreachable!(
                    "the parser only builds a quantified node for a comparison operator"
                ),
            };
            Ok((
                RExpr::Subquery {
                    plan: Box::new(plan),
                    kind: SubqueryKind::Quantified { op: cop, all: *all },
                    lhs: Some(Box::new(rlhs)),
                    negated: false,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::In { lhs, list, negated } => {
            // An EMPTY list reaches here only from folding an IN-subquery whose result was empty
            // (grammar.md §26; the parser rejects literal `IN ()` → 42601). The value is a constant
            // — `x IN (empty)` = FALSE, `x NOT IN (empty)` = TRUE — for every x including NULL.
            // Still resolve the LHS so an undefined column / aggregate-context error fires, then
            // return the constant (a leaf — no operator_eval, cost.md §3).
            if list.is_empty() {
                let _ = resolve(scope, lhs, None, agg, params)?;
                return Ok((RExpr::ConstBool(*negated), ResolvedType::Bool));
            }
            // Desugar to the OR-chain PostgreSQL DEFINES `IN` as: `x IN (a,b,c)` ≡
            // `x = a OR x = b OR x = c`; `NOT IN` is its negation (grammar.md §20). The list
            // is non-empty (the parser rejects `IN ()` → 42601). Resolving the desugared tree
            // reuses the `=`/OR/NOT machinery verbatim, so the three-valued NULL semantics,
            // per-element operand typing (a too-wide literal → 22003, a cross-family element →
            // 42804), and cost all fall out. The LHS is evaluated once per element (the
            // OR-chain model — a documented cost consequence, cost.md §3).
            let mut folded: Option<Expr> = None;
            for elem in list {
                let eq = binary_expr(BinaryOp::Eq, (**lhs).clone(), elem.clone());
                folded = Some(match folded {
                    None => eq,
                    Some(acc) => binary_expr(BinaryOp::Or, acc, eq),
                });
            }
            let mut desugared = folded.expect("IN list is non-empty (parser guarantees ≥1)");
            if *negated {
                desugared = Expr::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(desugared),
                };
            }
            resolve(scope, &desugared, ctx, agg, params)
        }
        Expr::Between {
            lhs,
            lo,
            hi,
            negated,
        } => {
            // Desugar to `lhs >= lo AND lhs <= hi` (grammar.md §21). The Kleene AND gives the PG
            // result for a NULL bound: `5 BETWEEN 10 AND NULL` is `FALSE AND NULL` = FALSE (a
            // FALSE operand dominates), while `5 BETWEEN 1 AND NULL` is `TRUE AND NULL` = NULL.
            // `NOT BETWEEN` negates the whole conjunction. The LHS is evaluated twice (the
            // desugar model — a documented cost consequence, cost.md §3).
            let ge = binary_expr(BinaryOp::Ge, (**lhs).clone(), (**lo).clone());
            let le = binary_expr(BinaryOp::Le, (**lhs).clone(), (**hi).clone());
            let mut desugared = binary_expr(BinaryOp::And, ge, le);
            if *negated {
                desugared = Expr::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(desugared),
                };
            }
            resolve(scope, &desugared, ctx, agg, params)
        }
        Expr::Like {
            lhs,
            rhs,
            negated,
            insensitive,
        } => {
            // LIKE / ILIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal
            // stays text), then require BOTH operands be text (or a bare NULL); a non-text
            // operand is 42804. We do NOT use classify_comparable here — it would wrongly accept
            // bytea×bytea, which LIKE does not define.
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            require_text_or_null(&lt)?;
            require_text_or_null(&rt)?;
            Ok((
                RExpr::Like {
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    negated: *negated,
                    insensitive: *insensitive,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::Regex {
            lhs,
            rhs,
            negated,
            insensitive,
        } => {
            // ~ / ~* / !~ / !~* — text×text → boolean (grammar.md §22b, regex.md). Same operand
            // typing as LIKE: resolve the pair, require both text (or a bare NULL); non-text 42804.
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            require_text_or_null(&lt)?;
            require_text_or_null(&rt)?;
            // Precompile a CONSTANT pattern ONCE (regex.md §5); a non-constant pattern compiles per
            // row at eval. For ~* the constant is case-folded before compiling (the ILIKE
            // mechanism); the subject is folded per row at eval. A malformed pattern surfaces 2201B
            // (and an oversized one 54001) here, at resolve, for the constant case.
            let program = if let RExpr::ConstText(pat) = &rr {
                let folded;
                let pat_ref = if *insensitive {
                    let prop = crate::collation::loaded_property();
                    folded = crate::collation::fold_lower_simple(pat, prop.as_deref());
                    folded.as_str()
                } else {
                    pat.as_str()
                };
                Some(crate::regex::compile(pat_ref)?)
            } else {
                None
            };
            // A precompiled program carries the one-shot `compile_charged` cost flag mutated on first
            // eval, so a reused plan would under-charge the 2nd+ execute — never cache such a plan.
            if program.is_some() {
                params.uncacheable = true;
            }
            Ok((
                RExpr::Regex {
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    negated: *negated,
                    insensitive: *insensitive,
                    program,
                    compile_charged: std::cell::Cell::new(false),
                },
                ResolvedType::Bool,
            ))
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            // Resolve each branch's condition: searched form requires a boolean WHEN (42804
            // otherwise); simple form desugars to `operand = value` (reusing the `=` operand
            // pairing + comparability check, so the value adapts to the operand's type). The
            // operand is evaluated once per tested branch (the desugar model, like IN).
            let mut arms: Vec<(RExpr, RExpr)> = Vec::with_capacity(whens.len());
            let mut result_types: Vec<ResolvedType> = Vec::with_capacity(whens.len() + 1);
            for (cond, res) in whens {
                let rcond = match operand {
                    Some(op) => {
                        let eq = binary_expr(BinaryOp::Eq, (**op).clone(), cond.clone());
                        resolve(scope, &eq, None, agg, params)?.0
                    }
                    None => {
                        let (rc, cty) = resolve(scope, cond, None, agg, params)?;
                        require_bool(&cty, "CASE WHEN condition must be boolean")?;
                        rc
                    }
                };
                let (rres, rty) = resolve(scope, res, None, agg, params)?;
                result_types.push(rty);
                arms.push((rcond, rres));
            }
            let (rels, ety) = match els {
                Some(e) => resolve(scope, e, None, agg, params)?,
                None => (RExpr::ConstNull, ResolvedType::Null),
            };
            result_types.push(ety);
            // Unify the THEN/ELSE result types into the CASE's common type (the render type).
            let unified = unify_case_types(&result_types, "CASE result types must be compatible")?;
            Ok((
                RExpr::Case {
                    arms,
                    els: Box::new(rels),
                    coerce_decimal: unified == ResolvedType::Decimal,
                },
                unified,
            ))
        }
        Expr::Coalesce(args) => {
            // COALESCE(a, b, …) (grammar.md §51): each argument resolves in the same agg context
            // (an aggregate argument is legal wherever an aggregate is), and the argument types
            // unify to one common type exactly like CASE's result arms.
            let mut rargs: Vec<RExpr> = Vec::with_capacity(args.len());
            let mut arg_types: Vec<ResolvedType> = Vec::with_capacity(args.len());
            for a in args {
                let (ra, aty) = resolve(scope, a, None, agg, params)?;
                rargs.push(ra);
                arg_types.push(aty);
            }
            let unified = unify_case_types(&arg_types, "COALESCE types must be compatible")?;
            Ok((
                RExpr::Coalesce {
                    args: rargs,
                    coerce_decimal: unified == ResolvedType::Decimal,
                },
                unified,
            ))
        }
        Expr::GreatestLeast { args, greatest } => {
            // GREATEST/LEAST(a, b, …) (grammar.md §52): each argument resolves in the same agg
            // context, and the argument types unify to one common ORDERABLE type. The winner is
            // chosen by that type's total order at eval, so — unlike CASE/COALESCE, which never
            // compare — the common type must actually be comparable and floats of mixed width
            // must be widened; this is why the unifier is `unify_minmax_types` (not the CASE
            // unifier) and why `classify_comparable` gates the result.
            let name = if *greatest { "greatest" } else { "least" };
            let mut rargs: Vec<RExpr> = Vec::with_capacity(args.len());
            let mut arg_types: Vec<ResolvedType> = Vec::with_capacity(args.len());
            for a in args {
                let (ra, aty) = resolve(scope, a, None, agg, params)?;
                rargs.push(ra);
                arg_types.push(aty);
            }
            let unified = unify_minmax_types(&arg_types, name)?;
            // The winner is chosen by the unified type's total order, so a non-orderable type
            // (json/jsonpath) or an incomparable pair is `42883`/`42804` HERE — never silently
            // mis-ordered by `value_cmp`'s cross-family totality fallback.
            classify_comparable(&unified, &unified)?;
            // A bare parameter takes the unified scalar type (like CASE/COALESCE — grammar.md §42).
            let hint = scalar_for_param_hint(&unified);
            for a in args {
                if let Expr::Param(n) = a {
                    params.note((*n as usize) - 1, hint)?;
                }
            }
            // A mixed-width float set unifies to f64; widen the f32 arguments so the comparator
            // sees a single width (the float island never mixes with int/decimal — §52).
            if unified == ResolvedType::Float(ScalarType::Float64) {
                rargs = rargs
                    .into_iter()
                    .zip(&arg_types)
                    .map(|(node, ty)| widen_float_to_f64(node, ty))
                    .collect();
            }
            // Text arguments derive one comparison collation (42P21/42P22 on conflict — §52).
            let collation = if unified == ResolvedType::Text {
                let mut deriv = Deriv::None;
                for a in args {
                    deriv = combine_deriv(deriv, derive_collation(scope, a)?)?;
                }
                resolve_deriv(scope.catalog, deriv)?
            } else {
                None
            };
            Ok((
                RExpr::GreatestLeast {
                    args: rargs,
                    coerce_decimal: unified == ResolvedType::Decimal,
                    greatest: *greatest,
                    collation,
                },
                unified,
            ))
        }
    }
}

/// Resolve a collation NAME to its loaded table (spec/design/collation.md §1). `C` is the built-in
/// byte / code-point order → `None` (the unchanged fast path); any other name must be loaded
/// (`db.import_collation`), else 42704.
pub(crate) fn resolve_collation_name(
    catalog: &Engine,
    name: &str,
) -> Result<Option<std::sync::Arc<Collation>>> {
    if name == "C" {
        return Ok(None);
    }
    match catalog.read_snap().resolve_collation(name) {
        Some(c) => Ok(Some(c)),
        None => Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("collation \"{name}\" does not exist"),
        )),
    }
}

/// A text expression's collation and its DERIVATION level (spec/design/collation.md §1, PostgreSQL's
/// rules). `None` ⇒ no collation (a non-text expr, or a bare literal — takes a neighbour's).
/// `Implicit(name)` ⇒ a column's frozen collation (`C` is a *distinct* implicit collation, so
/// `C`-vs-`en-US` conflicts — PG-matching). `Explicit(name)` ⇒ an explicit `COLLATE`.
/// `Indeterminate` ⇒ two different implicit collations met with no explicit override — an error only
/// when the collation is consumed (42P22, at `resolve_deriv`).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Deriv {
    None,
    Implicit(String),
    Explicit(String),
    Indeterminate,
}

/// Derive the collation + derivation level of a (text) expression subtree (spec/design/collation.md
/// §1). A `COLLATE` is explicit; a column reference is implicit (its frozen collation, `C` if none);
/// `||` combines its operands. Every other shape (literal, cast, function, CASE) resets to `None`
/// (no collation — takes a neighbour's), a documented narrowing (collation.md §14).
pub(crate) fn derive_collation(scope: &Scope, e: &Expr) -> Result<Deriv> {
    Ok(match e {
        Expr::Collate { collation, .. } => Deriv::Explicit(collation.clone()),
        Expr::Column(name) => column_deriv(scope, scope.resolve_bare(name).ok()),
        Expr::QualifiedColumn { qualifier, name } => {
            column_deriv(scope, scope.resolve_qualified(qualifier, name).ok())
        }
        Expr::Binary {
            op: BinaryOp::Concat,
            lhs,
            rhs,
        } => combine_deriv(derive_collation(scope, lhs)?, derive_collation(scope, rhs)?)?,
        _ => Deriv::None,
    })
}

/// The implicit derivation of a resolved column reference: a text column carries `Implicit(name)`
/// (its frozen collation, `C` if none); a non-text column or an unresolvable reference is `None`.
pub(crate) fn column_deriv(scope: &Scope, r: Option<Resolved>) -> Deriv {
    match r {
        Some(r) => {
            let col = scope.column_of(r);
            if col.ty.is_text() {
                Deriv::Implicit(col.collation.clone().unwrap_or_else(|| "C".to_string()))
            } else {
                Deriv::None
            }
        }
        None => Deriv::None,
    }
}

/// Combine two operands' derivations (spec/design/collation.md §1/§7, PG's rules). Explicit
/// dominates; two DIFFERENT explicit collations conflict eagerly (42P21); two different implicit
/// collations yield `Indeterminate` (deferred to 42P22 on use); explicit resolves an indeterminacy.
pub(crate) fn combine_deriv(a: Deriv, b: Deriv) -> Result<Deriv> {
    use Deriv::*;
    Ok(match (a, b) {
        (Explicit(x), Explicit(y)) => {
            if x != y {
                return Err(EngineError::new(
                    SqlState::CollationMismatch,
                    format!("collation mismatch between explicit collations \"{x}\" and \"{y}\""),
                ));
            }
            Explicit(x)
        }
        (Explicit(x), _) | (_, Explicit(x)) => Explicit(x),
        (Indeterminate, _) | (_, Indeterminate) => Indeterminate,
        (Implicit(x), Implicit(y)) => {
            if x == y {
                Implicit(x)
            } else {
                Indeterminate
            }
        }
        (Implicit(x), None) | (None, Implicit(x)) => Implicit(x),
        (None, None) => None,
    })
}

/// Resolve a derivation to the concrete collation a comparison / ORDER BY uses (spec/design/
/// collation.md §1/§7). `None` and `C` ⇒ `None` (byte order, the fast path); a loaded name ⇒ its
/// table (42704 if it vanished); `Indeterminate` ⇒ 42P22 (the collation is required but ambiguous).
pub(crate) fn resolve_deriv(
    catalog: &Engine,
    d: Deriv,
) -> Result<Option<std::sync::Arc<Collation>>> {
    match d {
        Deriv::None => Ok(None),
        Deriv::Implicit(name) | Deriv::Explicit(name) => resolve_collation_name(catalog, &name),
        Deriv::Indeterminate => Err(EngineError::new(
            SqlState::IndeterminateCollation,
            "could not determine which collation to use for string comparison",
        )),
    }
}

/// Compare two non-NULL text values under a loaded collation (spec/design/collation.md §6/§7): order
/// by the UCA sort keys, whose `memcmp` order IS the collation order. The caller charges the
/// `collate` cost and handles NULLs.
pub(crate) fn collated_cmp(coll: &Collation, a: &str, b: &str) -> Result<std::cmp::Ordering> {
    let ka = collation::sort_key(coll, a)?;
    let kb = collation::sort_key(coll, b)?;
    Ok(ka.cmp(&kb))
}
