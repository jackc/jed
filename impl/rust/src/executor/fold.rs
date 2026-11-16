//! Plan-time constant folding (spec/design/planner.md §3.7). A resolved expression's maximal closed
//! subtrees — an admitted interior node whose every operand is already a constant — are evaluated
//! once, when the expression is resolved, and replaced by the constant they produce. The boolean
//! connectives additionally drop a constant operand whose value cannot change the result.
//!
//! The pass is results-identical and never raises: an evaluation that fails (an error, the fold
//! meter's ceiling) leaves its subtree as written, so the error still surfaces only where execution
//! reaches it. It reads no row, parameter, session setting, clock, entropy, or catalog state: the
//! admitted node kinds and value domain below exclude every kernel that could.

use super::*;

/// Bounds one fold attempt's evaluation work (planner.md §3.7). An attempt that reaches it leaves its
/// subtree unfolded.
const FOLD_COST_LIMIT: i64 = 10000;

/// Fold `e` in place.
pub(crate) fn fold_constants(engine: &Engine, e: &mut RExpr) {
    fold_children(engine, e);
    if !fold_admitted(e) {
        return;
    }
    if fold_operands_constant(e) {
        let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: engine,
            params: &[],
            outer: &[],
            rng: &rng,
            ctes: CteCtx::empty(),
            host_batch: None,
        };
        let mut meter = Meter::with_limit(FOLD_COST_LIMIT);
        if let Ok(v) = e.eval(&[], &env, &mut meter) {
            if fold_value_in_domain(&v) {
                *e = value_to_rexpr(&v);
                return;
            }
        }
    }
    fold_connective(e);
}

/// Fold every child of `e` (bottom-up). A subquery body is resolved — and folded — by its own
/// planning; only its IN operand is walked here.
fn fold_children(engine: &Engine, e: &mut RExpr) {
    match e {
        RExpr::Subquery { lhs, .. } => {
            if let Some(l) = lhs {
                fold_constants(engine, l);
            }
        }
        RExpr::Cast { inner, .. }
        | RExpr::ArrayCast { inner, .. }
        | RExpr::CompositeCast { inner, .. } => fold_constants(engine, inner),
        RExpr::Neg { operand, .. } => fold_constants(engine, operand),
        RExpr::Not(x) => fold_constants(engine, x),
        RExpr::Casing { arg, .. } => fold_constants(engine, arg),
        RExpr::AtTimeZone { zone, value, .. } => {
            fold_constants(engine, zone);
            fold_constants(engine, value);
        }
        RExpr::DateTrunc { unit, value, zone } => {
            fold_constants(engine, unit);
            fold_constants(engine, value);
            if let Some(z) = zone {
                fold_constants(engine, z);
            }
        }
        RExpr::Extract { value, .. } => fold_constants(engine, value),
        RExpr::DateConvert { inner, .. } => fold_constants(engine, inner),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. } => {
            fold_constants(engine, lhs);
            fold_constants(engine, rhs);
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            fold_constants(engine, l);
            fold_constants(engine, r);
        }
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => {
            fold_constants(engine, base);
            fold_constants(engine, arg);
        }
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            fold_constants(engine, a);
            fold_constants(engine, b);
        }
        RExpr::IsNull { operand, .. }
        | RExpr::IsJson { operand, .. }
        | RExpr::JsonCtor { operand, .. } => fold_constants(engine, operand),
        RExpr::Case { arms, els, .. } => {
            for (c, res) in arms {
                fold_constants(engine, c);
                fold_constants(engine, res);
            }
            fold_constants(engine, els);
        }
        RExpr::Coalesce { args, .. }
        | RExpr::GreatestLeast { args, .. }
        | RExpr::NullIf { args, .. }
        | RExpr::ScalarFunc { args, .. }
        | RExpr::HostFunc { args, .. }
        | RExpr::ArrayFunc { args, .. }
        | RExpr::RangeFunc { args, .. }
        | RExpr::RegexFunc { args, .. }
        | RExpr::RangeCtor { args, .. }
        | RExpr::RangeOp { args, .. }
        | RExpr::RangeSetOp { args, .. }
        | RExpr::Variadic { args, .. }
        | RExpr::JsonBuild { args, .. }
        | RExpr::JsonSetInsert { args, .. }
        | RExpr::JsonObjectFromArrays { args, .. }
        | RExpr::JsonImage { args, .. }
        | RExpr::JsonPathFn { args, .. } => {
            for a in args {
                fold_constants(engine, a);
            }
        }
        RExpr::JsonSqlFn {
            ctx,
            path,
            passing,
            on_empty_default,
            on_error_default,
            ..
        } => {
            fold_constants(engine, ctx);
            fold_constants(engine, path);
            for p in passing
                .iter_mut()
                .chain(on_empty_default.iter_mut().map(|d| &mut **d))
                .chain(on_error_default.iter_mut().map(|d| &mut **d))
            {
                fold_constants(engine, p);
            }
        }
        RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
            for f in fields {
                fold_constants(engine, f);
            }
        }
        RExpr::Field { base, .. } => fold_constants(engine, base),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            fold_constants(engine, base);
            for s in subscripts {
                match s {
                    RSubscript::Index(i) => fold_constants(engine, i),
                    RSubscript::Slice { lower, upper } => {
                        if let Some(l) = lower {
                            fold_constants(engine, l);
                        }
                        if let Some(u) = upper {
                            fold_constants(engine, u);
                        }
                    }
                }
            }
        }
        RExpr::InValues { lhs, .. } => fold_constants(engine, lhs),
        RExpr::Quantified { lhs, array, .. }
        | RExpr::WidthBucketArray {
            operand: lhs,
            thresholds: array,
            ..
        } => {
            fold_constants(engine, lhs);
            fold_constants(engine, array);
        }
        RExpr::Column(_)
        | RExpr::OuterColumn { .. }
        | RExpr::Param(_)
        | RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstFloat32(_)
        | RExpr::ConstFloat64(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstJson(_)
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstJsonb(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        | RExpr::DateClock { .. }
        | RExpr::ConstNull => {}
    }
}

/// Drop a constant operand of AND/OR that cannot change the value (planner.md §3.7): a left operand
/// that decides the connective replaces it (the right operand is never evaluated, at run time
/// either); a left operand that does not decide it leaves the right operand; a right operand that
/// is the identity leaves the left. Each is exact under three-valued logic.
fn fold_connective(e: &mut RExpr) {
    let (decides, l, r) = match e {
        RExpr::And(l, r) => (false, l, r), // FALSE decides AND
        RExpr::Or(l, r) => (true, l, r),   // TRUE decides OR
        _ => return,
    };
    if let RExpr::ConstBool(b) = **l {
        *e = if b == decides {
            RExpr::ConstBool(decides)
        } else {
            std::mem::replace(&mut **r, RExpr::ConstNull)
        };
        return;
    }
    if let RExpr::ConstBool(b) = **r {
        if b != decides {
            *e = std::mem::replace(&mut **l, RExpr::ConstNull);
        }
    }
}

/// The admitted interior node set (planner.md §3.7).
fn fold_admitted(e: &RExpr) -> bool {
    matches!(
        e,
        RExpr::Cast { .. }
            | RExpr::Neg { .. }
            | RExpr::Not(_)
            | RExpr::Arith { .. }
            | RExpr::Compare { .. }
            | RExpr::And(..)
            | RExpr::Or(..)
            | RExpr::IsNull { .. }
            | RExpr::Distinct { .. }
            | RExpr::Case { .. }
            | RExpr::Coalesce { .. }
            | RExpr::GreatestLeast { .. }
            | RExpr::Array { .. }
    )
}

/// Whether every operand of an admitted node is a constant in the fold value domain.
fn fold_operands_constant(e: &RExpr) -> bool {
    match e {
        RExpr::Cast { inner, .. } => fold_constant(inner),
        RExpr::Neg { operand, .. } | RExpr::IsNull { operand, .. } => fold_constant(operand),
        RExpr::Not(x) => fold_constant(x),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. } => fold_constant(lhs) && fold_constant(rhs),
        RExpr::And(l, r) | RExpr::Or(l, r) => fold_constant(l) && fold_constant(r),
        RExpr::Case { arms, els, .. } => {
            arms.iter()
                .all(|(c, res)| fold_constant(c) && fold_constant(res))
                && fold_constant(els)
        }
        RExpr::Coalesce { args, .. } | RExpr::GreatestLeast { args, .. } => {
            args.iter().all(fold_constant)
        }
        RExpr::Array { elems, .. } => elems.iter().all(fold_constant),
        _ => false,
    }
}

/// Whether `e` is a constant node in the fold value domain.
fn fold_constant(e: &RExpr) -> bool {
    match e {
        RExpr::ConstNull
        | RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstFloat32(_)
        | RExpr::ConstFloat64(_) => true,
        RExpr::ConstArray(a) => fold_array_in_domain(&a.elements),
        _ => false,
    }
}

/// The fold value domain (planner.md §3.7): NULL, the integers, decimal, f32/f64, boolean, text,
/// bytea, uuid, and arrays of those.
fn fold_value_in_domain(v: &Value) -> bool {
    match v {
        Value::Null
        | Value::Int(_)
        | Value::Bool(_)
        | Value::Text(_)
        | Value::Decimal(_)
        | Value::Bytea(_)
        | Value::Uuid(_)
        | Value::Float32(_)
        | Value::Float64(_) => true,
        Value::Array(a) => fold_array_in_domain(&a.elements),
        _ => false,
    }
}

fn fold_array_in_domain(elements: &[Value]) -> bool {
    elements
        .iter()
        .all(|el| !matches!(el, Value::Array(_)) && fold_value_in_domain(el))
}
