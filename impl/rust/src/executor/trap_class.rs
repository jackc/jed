//! The planner's safe-conjunct gate (spec/design/planner.md §3.5) and the widened set-operation arm
//! rewrite (§3.6). Which casts and functions cannot trap is catalog data (functions.md §15):
//! casts.toml and catalog.toml classify them, and the codegen emits `NON_TRAPPING_CASTS` and the
//! operators' `traps` / `preserves_equality` fields. This module only reads that data.

use super::*;
use std::collections::HashMap;
use std::sync::OnceLock;

/// The trap classification of a cast or function (functions.md §15).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TrapClass {
    /// The node may raise an error (the default, for anything unclassified).
    #[default]
    May,
    /// The node raises nothing, and equal arguments give equal results.
    None,
    /// The node raises nothing, but may separate equal arguments (a decimal or float render:
    /// 1.5 = 1.50, -0 = 0).
    NoneLossy,
}

impl TrapClass {
    fn of(preserves_equality: bool) -> TrapClass {
        if preserves_equality {
            TrapClass::None
        } else {
            TrapClass::NoneLossy
        }
    }
}

/// The casts.toml name of a scalar resolved type (`None` for anything else).
fn resolved_scalar_name(t: &ResolvedType) -> Option<&'static str> {
    Some(match t {
        ResolvedType::Int(s) | ResolvedType::Float(s) => s.canonical_name(),
        ResolvedType::Decimal => "decimal",
        ResolvedType::Bool => "boolean",
        ResolvedType::Text => "text",
        ResolvedType::Uuid => "uuid",
        _ => return None,
    })
}

/// Classify a cast from a value of type `from` to `target` (casts.toml). A `numeric(p,s)` typmod can
/// overflow, so such a cast may trap whatever its row says.
pub(crate) fn cast_trap_class(
    from: &ResolvedType,
    target: ScalarType,
    typmod: Option<&DecimalTypmod>,
) -> TrapClass {
    if typmod.is_some() {
        return TrapClass::May;
    }
    let Some(name) = resolved_scalar_name(from) else {
        return TrapClass::May;
    };
    let to = target.canonical_name();
    crate::operators::NON_TRAPPING_CASTS
        .iter()
        .find(|c| c.from == name && c.to == to)
        .map_or(TrapClass::May, |c| TrapClass::of(c.preserves_equality))
}

/// Each catalog function name's (name-level) trap classification.
fn function_traps() -> &'static HashMap<&'static str, TrapClass> {
    static TABLE: OnceLock<HashMap<&'static str, TrapClass>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m: HashMap<&'static str, TrapClass> = HashMap::new();
        for op in crate::operators::OPERATORS {
            if op.kind != "function" {
                continue;
            }
            let mut c = if op.traps {
                TrapClass::May
            } else {
                TrapClass::of(op.preserves_equality)
            };
            // verify.rb forbids disagreeing overloads; stay conservative regardless.
            if m.get(op.name).is_some_and(|prev| *prev != c) {
                c = TrapClass::May;
            }
            m.insert(op.name, c);
        }
        m
    })
}

/// The trap classification of a built-in function call node, looked up by its catalog name (the
/// classification is name-level). Any other node is `May`.
fn function_trap_class(e: &RExpr) -> TrapClass {
    let name = match e {
        RExpr::ScalarFunc { func, .. } => explain_scalar_func(*func),
        RExpr::Casing { upper, .. } => (if *upper { "upper" } else { "lower" }).to_string(),
        _ => return TrapClass::May,
    };
    function_traps()
        .get(name.as_str())
        .copied()
        .unwrap_or(TrapClass::May)
}

/// A value node's own trap classification, ignoring its children: `None` for a column, parameter,
/// or literal; the cast's recorded class; a function's catalog class.
fn node_trap_class(e: &RExpr) -> TrapClass {
    match e {
        RExpr::Column(_) | RExpr::Param(_) => TrapClass::None,
        RExpr::Cast { trap, .. } => *trap,
        RExpr::ScalarFunc { .. } | RExpr::Casing { .. } => function_trap_class(e),
        _ if estimator_literal(e) => TrapClass::None,
        _ => TrapClass::May,
    }
}

/// Whether a value expression can raise no error (planner.md §3.5): a column, literal, or
/// parameter, or a classified cast or function call over non-trapping arguments.
pub(crate) fn non_trapping(e: &RExpr) -> bool {
    node_trap_class(e) != TrapClass::May
        && estimator_expression_children(e)
            .into_iter()
            .all(non_trapping)
}

/// Whether a pushdown-safe conjunct contains a cast or function that may give different results for
/// equal values (`NoneLossy`), which keeps it out of bodies that match rows (planner.md §3.2).
pub(crate) fn conjunct_lossy(e: &RExpr) -> bool {
    (matches!(
        e,
        RExpr::Cast { .. } | RExpr::ScalarFunc { .. } | RExpr::Casing { .. }
    ) && node_trap_class(e) == TrapClass::NoneLossy)
        || estimator_expression_children(e)
            .into_iter()
            .any(conjunct_lossy)
}

/// The plan-time LIKE pattern proof (planner.md §3.5): the pattern is a plan-time literal (a
/// literal, or a parameter read through the custom-plan frame) that is NULL or text whose escape
/// characters all pair, so the matcher can never raise 22025.
pub(crate) fn like_pattern_proven(p: &RExpr) -> bool {
    if !estimator_literal(p) {
        return false;
    }
    if estimator_is_null(p) {
        return true;
    }
    estimator_value(p)
        .is_some_and(|v| matches!(v.as_ref(), RExpr::ConstText(s) if like_pattern_well_formed(s)))
}

/// Whether every `\` in a LIKE pattern, read left to right in pairs, is followed by another
/// character — the only condition under which `like_match` raises 22025.
fn like_pattern_well_formed(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.next().is_none() {
            return false;
        }
    }
    true
}

/// How a set-operation arm column of type `leaf` reaches the unified output type `want`
/// (planner.md §3.6): `Some(None)` with no value change, `Some(Some(target))` through a widening
/// cast to `target`, `None` not at all.
pub(crate) fn arm_widening(leaf: &ResolvedType, want: &ResolvedType) -> Option<Option<ScalarType>> {
    if value_noop_widening(leaf, want) {
        return Some(None);
    }
    match (leaf, want) {
        (ResolvedType::Int(_), ResolvedType::Decimal) => Some(Some(ScalarType::Decimal)),
        (ResolvedType::Float(ScalarType::Float32), ResolvedType::Float(ScalarType::Float64)) => {
            Some(Some(ScalarType::Float64))
        }
        _ => None,
    }
}

/// Whether the set operation's conversion from `leaf` to `want` leaves every value unchanged: the
/// same type, integer widths (alone or inside arrays and anonymous records), or a NULL-typed column.
fn value_noop_widening(leaf: &ResolvedType, want: &ResolvedType) -> bool {
    match (leaf, want) {
        _ if leaf == want => true,
        (ResolvedType::Null, _) | (ResolvedType::Int(_), ResolvedType::Int(_)) => true,
        (ResolvedType::Array(l), ResolvedType::Array(w)) => value_noop_widening(l, w),
        (ResolvedType::Composite(l), ResolvedType::Composite(w))
            if l.name.is_none() && w.name.is_none() =>
        {
            l.fields.len() == w.fields.len()
                && l.fields
                    .iter()
                    .zip(&w.fields)
                    .all(|((_, lt), (_, wt))| value_noop_widening(lt, wt))
        }
        _ => false,
    }
}

/// Whether a body never matches rows by value (planner.md §3.2): every SELECT is ungrouped with no
/// DISTINCT and no window function, combined only by UNION ALL. A lossy conjunct may move only into
/// such a body.
pub(crate) fn non_matching_body(q: &QueryPlan) -> bool {
    match q {
        QueryPlan::Select(b) => !b.is_agg && !b.distinct && b.distinct_on == 0 && !b.has_window,
        QueryPlan::SetOp(so) => {
            so.op == SetOpKind::Union
                && so.all
                && non_matching_body(&so.lhs)
                && non_matching_body(&so.rhs)
        }
        _ => false,
    }
}
