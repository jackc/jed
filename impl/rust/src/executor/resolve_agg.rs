//! Aggregate/window and container-function resolution (mirrors impl/go aggregate_resolve.go plus part of
//! resolve_func.go): GROUP BY term resolution, array/range function resolution, and the aggregate/window
//! function-call resolvers (resolve_aggregate/resolve_grouping/resolve_window_call/resolve_frame).

use super::*;

/// Resolve one `GROUP BY` grouping term to a column or a materialized expression (aggregates.md §15).
/// Classifies the term: a bare integer literal is a select-list ORDINAL (1-based; out of range
/// 42P10) whose target select item is then resolved as a term; otherwise it is a column / alias /
/// general expression (`resolve_group_named`).
pub(crate) fn resolve_group_term(
    scope: &Scope,
    term: &Expr,
    items: &SelectItems,
    params: &mut ParamTypes,
) -> Result<GroupKeyResolved> {
    // Only a *bare* integer literal is an ordinal — `GROUP BY 1`; `GROUP BY 1 + 1` is a constant
    // expression (PG). The parser folds a unary minus into the value, so a negative is just out of
    // range. The select list fixes the position count: `*` expands to the scope width.
    if let Expr::Literal(Literal::Int(n), _) = term {
        let ncols = match items {
            SelectItems::All(_) => scope.width() as i64,
            SelectItems::Items(its) => its.len() as i64,
        };
        if *n < 1 || *n > ncols {
            return Err(EngineError::new(
                SqlState::InvalidColumnReference,
                format!("GROUP BY position {n} is not in select list"),
            ));
        }
        let pos = (*n - 1) as usize;
        return match items {
            // `SELECT *` — the ordinal names the column at that scope position directly.
            SelectItems::All(_) => Ok(GroupKeyResolved::Column(pos)),
            SelectItems::Items(its) => resolve_group_expr(scope, &its[pos].expr, params),
        };
    }
    resolve_group_named(scope, term, items, params)
}

/// Resolve a non-ordinal grouping term: a bare/qualified column, an output alias, or a general
/// expression (aggregates.md §15). A bare name resolves an INPUT column FIRST, then — only if there
/// is no such column — an output alias (PG's rule, the opposite of `ORDER BY`'s output-first rule).
pub(crate) fn resolve_group_named(
    scope: &Scope,
    term: &Expr,
    items: &SelectItems,
    params: &mut ParamTypes,
) -> Result<GroupKeyResolved> {
    match term {
        Expr::Column(name, pos) => match scope.resolve_bare_at(name, *pos) {
            Ok(Resolved::Local(idx)) => Ok(GroupKeyResolved::Column(idx)),
            // An enclosing-query column is a per-evaluation constant: materialize it as an
            // expression key (query.group_by_correlated, grammar.md §26).
            Ok(Resolved::Outer { .. } | Resolved::Layer { .. }) => {
                resolve_group_expr(scope, term, params)
            }
            // No input column of this name: try an output alias (`SELECT a+b AS s … GROUP BY s`).
            // If none matches either, propagate the original 42703.
            Err(e) if e.state == SqlState::UndefinedColumn => {
                match order_alias_match(items, name, scope)? {
                    Some(aexpr) => resolve_group_expr(scope, aexpr, params),
                    None => Err(e),
                }
            }
            Err(e) => Err(e),
        },
        Expr::QualifiedColumn {
            qualifier,
            name,
            pos,
        } => match scope.resolve_qualified_at(qualifier, name, *pos)? {
            Resolved::Local(idx) => Ok(GroupKeyResolved::Column(idx)),
            Resolved::Outer { .. } | Resolved::Layer { .. } => {
                resolve_group_expr(scope, term, params)
            }
        },
        _ => resolve_group_expr(scope, term, params),
    }
}

/// Resolve a grouping expression (the target of an ordinal/alias, or a general `GROUP BY a+b`). A
/// plain column expression stays a COLUMN key (so the projection's bare-column path matches it);
/// anything else is MATERIALIZED — resolved against the input row with aggregates forbidden (an
/// aggregate in GROUP BY is 42803), its canonical AST kept for projection matching (aggregates.md §15).
pub(crate) fn resolve_group_expr(
    scope: &Scope,
    e: &Expr,
    params: &mut ParamTypes,
) -> Result<GroupKeyResolved> {
    match e {
        Expr::Column(name, pos) => {
            if let Resolved::Local(idx) = scope.resolve_bare_at(name, *pos)? {
                return Ok(GroupKeyResolved::Column(idx));
            }
        }
        Expr::QualifiedColumn {
            qualifier,
            name,
            pos,
        } => {
            if let Resolved::Local(idx) = scope.resolve_qualified_at(qualifier, name, *pos)? {
                return Ok(GroupKeyResolved::Column(idx));
            }
        }
        _ => {}
    }
    let mut sub = AggCtx::Forbidden;
    let (rexpr, ty) = resolve(scope, e, None, &mut sub, params)?;
    Ok(GroupKeyResolved::Expr(rexpr, ty, e.clone()))
}

/// If `e` structurally matches a general-expression `GROUP BY` key in this aggregate context, return
/// that group's synthetic key slot (its master position) and resolved type (aggregates.md §15). Only
/// fires in `Collect` / `GroupedWindow`; an aggregate operand / FILTER resolves under `Forbidden`, so
/// a grouping expression there is correctly NOT remapped (it is a per-row value, not the group key).
pub(crate) fn match_group_expr(agg: &AggCtx, e: &Expr) -> Option<(usize, ResolvedType)> {
    let gke = match agg {
        AggCtx::Collect {
            group_key_exprs, ..
        }
        | AggCtx::GroupedWindow {
            group_key_exprs, ..
        } => group_key_exprs,
        _ => return None,
    };
    gke.iter().enumerate().find_map(|(p, gk)| match gk {
        Some((ge, ty, _)) if ge == e => Some((p, ty.clone())),
        _ => None,
    })
}

/// If the enclosing-query column at `(level, index)` is a `GROUP BY` key in this aggregate context,
/// return that group's master position and type (query.group_by_correlated).
pub(crate) fn match_group_outer(
    agg: &AggCtx,
    level: usize,
    index: usize,
) -> Option<(usize, ResolvedType)> {
    let gke = match agg {
        AggCtx::Collect {
            group_key_exprs, ..
        }
        | AggCtx::GroupedWindow {
            group_key_exprs, ..
        } => group_key_exprs,
        _ => return None,
    };
    gke.iter().enumerate().find_map(|(p, gk)| match gk {
        Some((_, ty, Some(o))) if *o == (level, index) => Some((p, ty.clone())),
        _ => None,
    })
}

/// Compute a `GROUPING(args)` result for a group from the grouping set whose `mask` is given: bit
/// `(k-1-j)` of the result is bit `positions[j]` of `mask` (1 iff that column is grouped away in this
/// set). spec/design/aggregates.md §12.
pub(crate) fn grouping_value(positions: &[usize], mask: i64) -> i64 {
    let k = positions.len();
    let mut r: i64 = 0;
    for (j, &p) in positions.iter().enumerate() {
        let bit = (mask >> p) & 1;
        r |= bit << (k - 1 - j);
    }
    r
}

/// Rewrite placeholder column slots in `[base, 2·base)` (a window-result `WINDOW_RESULT_BASE + w` or
/// a `GROUPING(...)` `GROUPING_GS_BASE + g`) to their real synthetic slot `target + (slot - base)`,
/// once the grouped/windowed row layout is final (spec/design/window.md §5.1, aggregates.md §12/§21).
/// Each placeholder base is 2× the previous (`1<<28`, `1<<29`, `1<<30`) and a base's placeholder
/// count is far below that gap, so bounding the rewrite to `[base, 2·base)` keeps the bases isolated —
/// a window-result rebase no longer clobbers a `GROUPING()` placeholder (the two now COEXIST in a
/// GROUPING SETS + window query — aggregates.md §21).
pub(crate) fn rebase_placeholder_cols(e: &mut RExpr, from: usize, target: usize) {
    match e {
        RExpr::Column(i) => {
            if *i >= from && *i < from * 2 {
                *i = target + (*i - from);
            }
        }
        RExpr::Subquery { lhs, .. } => {
            if let Some(l) = lhs {
                rebase_placeholder_cols(l, from, target);
            }
        }
        RExpr::InValues { lhs, .. } => rebase_placeholder_cols(lhs, from, target),
        RExpr::Quantified { lhs, array, .. }
        | RExpr::WidthBucketArray {
            operand: lhs,
            thresholds: array,
            ..
        } => {
            rebase_placeholder_cols(lhs, from, target);
            rebase_placeholder_cols(array, from, target);
        }
        RExpr::Cast { inner, .. } | RExpr::ArrayCast { inner, .. } => {
            rebase_placeholder_cols(inner, from, target)
        }
        RExpr::Neg { operand, .. } => rebase_placeholder_cols(operand, from, target),
        RExpr::Not(x) => rebase_placeholder_cols(x, from, target),
        RExpr::Casing { arg, .. } => rebase_placeholder_cols(arg, from, target),
        RExpr::AtTimeZone { zone, value, .. } => {
            rebase_placeholder_cols(zone, from, target);
            rebase_placeholder_cols(value, from, target);
        }
        RExpr::DateTrunc { unit, value, zone } => {
            rebase_placeholder_cols(unit, from, target);
            rebase_placeholder_cols(value, from, target);
            if let Some(z) = zone {
                rebase_placeholder_cols(z, from, target);
            }
        }
        RExpr::Extract { value, .. } => rebase_placeholder_cols(value, from, target),
        RExpr::DateConvert { inner, .. } => rebase_placeholder_cols(inner, from, target),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. }
        | RExpr::Regex { lhs, rhs, .. } => {
            rebase_placeholder_cols(lhs, from, target);
            rebase_placeholder_cols(rhs, from, target);
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            rebase_placeholder_cols(l, from, target);
            rebase_placeholder_cols(r, from, target);
        }
        RExpr::IsNull { operand, .. } => rebase_placeholder_cols(operand, from, target),
        RExpr::Case { arms, els, .. } => {
            for (c, r) in arms {
                rebase_placeholder_cols(c, from, target);
                rebase_placeholder_cols(r, from, target);
            }
            rebase_placeholder_cols(els, from, target);
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
        | RExpr::JsonSetInsert { args, .. }
        | RExpr::JsonObjectFromArrays { args, .. }
        | RExpr::JsonImage { args, .. }
        | RExpr::JsonPathFn { args, .. }
        | RExpr::JsonBuild { args, .. } => {
            for a in args {
                rebase_placeholder_cols(a, from, target);
            }
        }
        RExpr::Row(fields) | RExpr::Array { elems: fields, .. } => {
            for f in fields {
                rebase_placeholder_cols(f, from, target);
            }
        }
        RExpr::Field { base, .. } => rebase_placeholder_cols(base, from, target),
        RExpr::Subscript {
            base, subscripts, ..
        } => {
            rebase_placeholder_cols(base, from, target);
            for s in subscripts.iter_mut() {
                match s {
                    RSubscript::Index(i) => rebase_placeholder_cols(i, from, target),
                    RSubscript::Slice { lower, upper } => {
                        if let Some(l) = lower {
                            rebase_placeholder_cols(l, from, target);
                        }
                        if let Some(u) = upper {
                            rebase_placeholder_cols(u, from, target);
                        }
                    }
                }
            }
        }
        RExpr::JsonGet { base, arg, .. }
        | RExpr::JsonHasKey { base, arg, .. }
        | RExpr::JsonDelete { base, arg, .. } => {
            rebase_placeholder_cols(base, from, target);
            rebase_placeholder_cols(arg, from, target);
        }
        RExpr::JsonContains { a, b } | RExpr::JsonConcat { a, b } => {
            rebase_placeholder_cols(a, from, target);
            rebase_placeholder_cols(b, from, target);
        }
        RExpr::JsonSqlFn {
            ctx, path, passing, ..
        } => {
            rebase_placeholder_cols(ctx, from, target);
            rebase_placeholder_cols(path, from, target);
            for p in passing {
                rebase_placeholder_cols(p, from, target);
            }
        }
        RExpr::IsJson { operand, .. } | RExpr::JsonCtor { operand, .. } => {
            rebase_placeholder_cols(operand, from, target)
        }
        RExpr::OuterColumn { .. }
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
        | RExpr::ConstJsonb(_)
        | RExpr::ConstJsonPath(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstDate(_)
        | RExpr::ConstInterval(_)
        | RExpr::ConstArray(_)
        | RExpr::ConstRange(_)
        // A DateClock leaf carries no column slots.
        | RExpr::DateClock { .. }
        | RExpr::ConstNull => {}
    }
}

/// Walk a nested plan's expression surfaces for outer references back into the target scope —
/// the same five surfaces `select_plan_references_outer` checks (slot lists like group keys /
/// ORDER BY index the nested plan's own rows and can never reach outward).
pub(crate) fn collect_touched_plan(plan: &QueryPlan, depth: usize, touched: &mut [bool]) {
    match plan {
        QueryPlan::Select(sp) => {
            for j in &sp.joins {
                if let Some(on) = &j.on {
                    collect_touched(on, depth, touched);
                }
            }
            if let Some(f) = sp.filter.get() {
                collect_touched(f, depth, touched);
            }
            if let Some(h) = &sp.having {
                collect_touched(h, depth, touched);
            }
            for s in &sp.agg_specs {
                if let Some(op) = &s.operand {
                    collect_touched(op, depth, touched);
                }
                if let Some(order) = &s.order {
                    for k in &order.keys {
                        collect_touched(k, depth, touched);
                    }
                }
            }
            for p in &sp.projections {
                collect_touched(p, depth, touched);
            }
            // A materialized ORDER BY expression and a set-returning relation's args / a LATERAL derived
            // body can each carry a correlated reference back into the target scope (the same surfaces
            // select_plan_references_outer checks — query.order_by_correlated, functions.md §10,
            // grammar.md §44). collect_touched_plan MUST cover every surface that function does, or an
            // outer column read only through one of them is left unfetched by the lazy/masked scan
            // (large-values.md §14) and the correlated subquery re-executes against NULL — a
            // memory-vs-disk divergence.
            for oe in &sp.order_exprs {
                collect_touched(oe, depth, touched);
            }
            for ge in &sp.group_exprs {
                collect_touched(ge, depth, touched);
            }
            for r in &sp.rels {
                if let Some(srf) = &r.srf {
                    for a in &srf.args {
                        collect_touched(a, depth, touched);
                    }
                }
                if let Some(derived) = &r.derived {
                    collect_touched_plan(derived, depth + r.derived_body_frame(), touched);
                }
            }
        }
        QueryPlan::SetOp(s) => {
            collect_touched_plan(&s.lhs, depth, touched);
            collect_touched_plan(&s.rhs, depth, touched);
        }
        QueryPlan::Values(vp) => {
            for row in &vp.rows {
                for e in row {
                    collect_touched(e, depth, touched);
                }
            }
        }
        // A nested `WITH`'s correlated references live in its body and its CTE bodies, all planned
        // against the same enclosing scope; recurse into each at the same depth (cte.md §7).
        QueryPlan::With(wp) => {
            collect_touched_plan(&wp.body, depth, touched);
            for p in wp.binding_plans() {
                collect_touched_plan(p, depth, touched);
            }
        }
    }
}

/// Three-valued `lhs IN (list)` membership (spec/design/grammar.md §26), charging one
/// `operator_eval` per element compared. An EMPTY list is `negated` (`x IN ()` = FALSE,
/// `x NOT IN ()` = TRUE) independent of `lv`. Otherwise: a positive match → TRUE; else a NULL
/// element (or NULL `lv`) → NULL (unknown); else FALSE. `NOT IN` is the Kleene negation. Shared
/// by the folded `InValues` node and the correlated `Subquery { In }` eval.
pub(crate) fn in_membership(
    lv: &Value,
    list: &[Value],
    negated: bool,
    m: &mut Meter,
) -> Result<Value> {
    if list.is_empty() {
        return Ok(Value::Bool(negated));
    }
    let mut any_match = false;
    let mut any_null = false;
    for v in list {
        m.charge(COSTS.operator_eval);
        // Each element comparison over a decimal pair charges its size-scaled decimal_work
        // (cost.md §3 "decimal_work"), like a Compare node.
        m.charge(COSTS.decimal_work * ((decimal_cmp_work(lv, v) - 1) as i64));
        m.guard()?;
        match lv.eq3(v) {
            ThreeValued::True => any_match = true,
            ThreeValued::Unknown => any_null = true,
            ThreeValued::False => {}
        }
    }
    let in_val = if any_match {
        Value::Bool(true)
    } else if any_null {
        Value::Null
    } else {
        Value::Bool(false)
    };
    Ok(if negated { not3(&in_val) } else { in_val })
}

/// Build a binary-operator `Expr` node (used by the IN/BETWEEN desugar in `resolve`). `pos` is the
/// desugared construct's operator position — PostgreSQL reports an IN / BETWEEN comparison's
/// operator error there (spec/design/error-fields.md §10.2).
pub(crate) fn binary_expr(op: BinaryOp, lhs: Expr, rhs: Expr, pos: SrcPos) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        pos,
    }
}

/// The `USING` column list a `NATURAL` join derives (spec/design/grammar.md §15): the column names
/// common to the join's LEFT input (`left_names`, its `*` columns in output order) and the right
/// relation, in LEFT order with each name taken once (its first occurrence). An empty result
/// degenerates the join to a `CROSS` join. (A merged column on the left keeps its underlying name,
/// so a re-merge via a NATURAL chain is found here too.)
pub(crate) fn natural_common_cols(left_names: &[&str], right: &ScopeRel) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for &name in left_names {
        if seen.insert(name.to_string()) && right.table.column_index(name).is_some() {
            out.push(name.to_string());
        }
    }
    out
}

// === Function registry (spec/design/extensibility.md §5) ============================
// Resolution for the named scalar functions and the aggregates is DATA-DRIVEN: instead of
// re-encoding the name set in hand-written `match`es (the old known-name gate + result-type
// match + name→variant match), it consults the generated catalog descriptor tables
// (`OPERATORS` rows with kind="function", and `AGGREGATES`) through the lookups below, keyed
// by (name, arg_families). The per-row KERNEL is still reached by id (`ScalarFunc` / `AggPlan`)
// and hand-written per core — §5 forbids codegenning the kernels. The only function-specific
// hand-written datum is `scalar_func_id` (name → kernel id); `registry_covers_catalog` (test)
// proves it total over the catalog. Host-registered functions would extend these lookups.

/// The argument family a resolved type satisfies, for matching a catalog `arg_families` slot.
/// `None` for NULL: an untyped NULL matches no *concrete* family — so `abs(NULL)` / `sum(NULL)`
/// find no overload (42883, the pre-registry behavior) — and only the wildcard "any" accepts it.
pub(crate) fn arg_family(t: &ResolvedType) -> Option<&'static str> {
    match t {
        ResolvedType::Int(_) => Some("integer"),
        ResolvedType::Decimal => Some("decimal"),
        ResolvedType::Float(_) => Some("float"),
        ResolvedType::Bool => Some("boolean"),
        ResolvedType::Text => Some("text"),
        ResolvedType::Bytea => Some("bytea"),
        ResolvedType::Uuid => Some("uuid"),
        ResolvedType::Timestamp => Some("timestamp"),
        ResolvedType::Timestamptz => Some("timestamptz"),
        ResolvedType::Date => Some("date"),
        ResolvedType::Interval => Some("interval"),
        ResolvedType::Json => Some("json"),
        ResolvedType::JsonPath => Some("jsonpath"),
        ResolvedType::Jsonb => Some("jsonb"),
        ResolvedType::Null => None,
        // A composite/array/range is no concrete built-in argument family this slice. (A range's
        // polymorphic `anyrange` family is matched separately by the range resolver — RF1.)
        ResolvedType::Composite(_) | ResolvedType::Array(_) | ResolvedType::Range(_) => None,
    }
}

/// Whether a resolved argument satisfies one catalog family slot. "any" accepts everything
/// (NULL included); a concrete family matches only its own type.
pub(crate) fn family_matches(slot: &str, t: &ResolvedType) -> bool {
    if slot == "numeric" {
        // The pseudo-family of the two-argument statistical aggregates (aggregates.md §23): any
        // integer, decimal, or float argument, widened to f64.
        return matches!(
            t,
            ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Float(_)
        );
    }
    slot == "any" || arg_family(t) == Some(slot)
}

/// Whether `name` (exact, grammar.md §3) is a registered scalar function (catalog kind="function").
/// This is the data-driven replacement for the old hand-written known-name gate.
pub(crate) fn is_scalar_func_name(name: &str) -> bool {
    OPERATORS
        .iter()
        .any(|o| o.kind == "function" && o.name == name)
}

/// Whether `name` (exact, grammar.md §3) is a VARIADIC scalar function (array-functions.md §12) — a
/// `kind="function"` row with `variadic = true` (`num_nulls`/`num_nonnulls`). Data-driven, so
/// adding a variadic row to the catalog wires it here without touching this gate.
pub(crate) fn is_variadic_func_name(name: &str) -> bool {
    OPERATORS
        .iter()
        .any(|o| o.kind == "function" && o.variadic && o.name == name)
}

/// The matched scalar-function overload row for `name` over the resolved argument types: the
/// `kind="function"` catalog row whose `arg_families` agree by arity + per-slot family. `None`
/// ⇒ no overload (42883). `make_interval` resolves on its own named/defaulted path (§11).
pub(crate) fn lookup_scalar_overload(
    name: &str,
    arg_tys: &[ResolvedType],
) -> Option<&'static OperatorDesc> {
    OPERATORS.iter().find(|o| {
        o.kind == "function"
            && o.name == name
            && o.arg_families.len() == arg_tys.len()
            && std::iter::zip(o.arg_families, arg_tys).all(|(slot, t)| family_matches(slot, t))
    })
}

/// Select the scalar-function overload for `name` over the resolved argument types, where an
/// argument of the untyped NULL type (a bare NULL, or a `$N` with no context) is unknown-typed and
/// adopts a slot's family (functions.md §9.1). `Ok(None)` when no built-in row matches (the caller
/// tries the host registry, then 42883); 42725 when the overloads cannot decide.
pub(crate) fn select_scalar_overload(
    name: &str,
    arg_tys: &[ResolvedType],
) -> Result<Option<&'static OperatorDesc>> {
    if !arg_tys.iter().any(|t| matches!(t, ResolvedType::Null)) {
        return Ok(lookup_scalar_overload(name, arg_tys));
    }
    select_unknown_overload(name, arg_tys, None)
}

/// Whether a bare string literal can be read as a value of a slot's family: the families whose input
/// the literal resolver converts (`resolve_node`'s `Literal::Text` arm), plus `text` itself and the
/// unconstrained `any`. The numeric and boolean families are not among them.
pub(crate) fn literal_adapts(slot: &str) -> bool {
    matches!(
        slot,
        "any"
            | "text"
            | "bytea"
            | "uuid"
            | "date"
            | "timestamp"
            | "timestamptz"
            | "interval"
            | "json"
            | "jsonb"
            | "jsonpath"
    )
}

/// The unknown-argument rule of functions.md §9.1. An argument of the untyped NULL type is unknown
/// and matches any slot; so is a position marked in `literals` (a bare string literal), which
/// matches only the slots `literal_adapts` admits.
pub(crate) fn select_unknown_overload(
    name: &str,
    arg_tys: &[ResolvedType],
    literals: Option<&[bool]>,
) -> Result<Option<&'static OperatorDesc>> {
    let literal = |i: usize| literals.is_some_and(|l| l[i]);
    let unknown = |i: usize| matches!(arg_tys[i], ResolvedType::Null) || literal(i);
    let candidates: Vec<&'static OperatorDesc> = OPERATORS
        .iter()
        .filter(|o| {
            o.kind == "function"
                && o.name == name
                && o.arg_families.len() == arg_tys.len()
                && o.arg_families.iter().enumerate().all(|(i, slot)| {
                    if literal(i) {
                        literal_adapts(slot)
                    } else {
                        unknown(i) || family_matches(slot, &arg_tys[i])
                    }
                })
        })
        .collect();
    if candidates.len() <= 1 {
        return Ok(candidates.first().copied());
    }
    // At each unknown position pick `text` if any candidate accepts it, else the one family every
    // candidate agrees on; a disagreement without `text` is ambiguous.
    let mut picked: Vec<Option<&str>> = vec![None; arg_tys.len()];
    for i in 0..arg_tys.len() {
        if !unknown(i) {
            continue;
        }
        let first = candidates[0].arg_families[i];
        picked[i] = if candidates.iter().any(|o| o.arg_families[i] == "text") {
            Some("text")
        } else if candidates.iter().all(|o| o.arg_families[i] == first) {
            Some(first)
        } else {
            return Err(ambiguous_func_overload(name));
        };
    }
    let kept: Vec<&'static OperatorDesc> = candidates
        .into_iter()
        .filter(|o| {
            picked
                .iter()
                .enumerate()
                .all(|(i, p)| p.is_none_or(|f| o.arg_families[i] == f))
        })
        .collect();
    match kept.as_slice() {
        [one] => Ok(Some(*one)),
        _ => Err(ambiguous_func_overload(name)),
    }
}

/// The concrete type an unknown-typed bind parameter adopts in a function slot of `family`
/// (functions.md §9.1): the integer family's default `i64`, the float family's `f64`, and each
/// single-type family its own scalar. `None` for `any` (and the polymorphic pseudo-families).
pub(crate) fn family_param_type(family: &str) -> Option<ScalarType> {
    Some(match family {
        "numeric" => ScalarType::Float64,
        "integer" => ScalarType::Int64,
        "float" => ScalarType::Float64,
        "decimal" => ScalarType::Decimal,
        "text" => ScalarType::Text,
        "boolean" => ScalarType::Bool,
        "bytea" => ScalarType::Bytea,
        "uuid" => ScalarType::Uuid,
        "date" => ScalarType::Date,
        "timestamp" => ScalarType::Timestamp,
        "timestamptz" => ScalarType::Timestamptz,
        "interval" => ScalarType::Interval,
        "json" => ScalarType::Json,
        "jsonb" => ScalarType::Jsonb,
        "jsonpath" => ScalarType::JsonPath,
        _ => return None,
    })
}

/// The kernel id for scalar function `name` — the per-core hand-written half of the registry
/// (§5: the kernel is reached by id, never codegenned). Total over the catalog's function names
/// (`registry_covers_catalog` proves it); for Rust the id depends only on the name (one `Abs`
/// arm serves int/decimal/float; one `Round` arm serves float/decimal — the eval recovers the
/// overload from the operand value).
pub(crate) fn scalar_func_id(name: &str) -> ScalarFunc {
    match name {
        "abs" => ScalarFunc::Abs,
        "round" => ScalarFunc::Round,
        "ceil" => ScalarFunc::Ceil,
        "ceiling" => ScalarFunc::Ceil, // alias of ceil (same kernel)
        "floor" => ScalarFunc::Floor,
        "trunc" => ScalarFunc::Trunc,
        "sqrt" => ScalarFunc::Sqrt,
        "exp" => ScalarFunc::Exp,
        "ln" => ScalarFunc::Ln,
        // `log` is decimal-only (1-arg base-10 / 2-arg arbitrary-base); `log10` keeps its own id.
        "log" => ScalarFunc::Log,
        "log10" => ScalarFunc::Log10,
        // `power` is PG's name for `pow` (the documented name gap) — same kernel.
        "pow" | "power" => ScalarFunc::Pow,
        "sin" => ScalarFunc::Sin,
        "cos" => ScalarFunc::Cos,
        "tan" => ScalarFunc::Tan,
        "cbrt" => ScalarFunc::Cbrt,
        "pi" => ScalarFunc::Pi,
        "radians" => ScalarFunc::Radians,
        "degrees" => ScalarFunc::Degrees,
        "asin" => ScalarFunc::Asin,
        "acos" => ScalarFunc::Acos,
        "atan" => ScalarFunc::Atan,
        "atan2" => ScalarFunc::Atan2,
        "cot" => ScalarFunc::Cot,
        "sinh" => ScalarFunc::Sinh,
        "cosh" => ScalarFunc::Cosh,
        "tanh" => ScalarFunc::Tanh,
        "asinh" => ScalarFunc::Asinh,
        "acosh" => ScalarFunc::Acosh,
        "atanh" => ScalarFunc::Atanh,
        "sign" => ScalarFunc::Sign,
        "factorial" => ScalarFunc::Factorial,
        "scale" => ScalarFunc::Scale,
        "min_scale" => ScalarFunc::MinScale,
        "trim_scale" => ScalarFunc::TrimScale,
        "make_interval" => ScalarFunc::MakeInterval,
        // make_timestamp / make_timestamptz resolve on their own named/un-defaulted path (§11), like
        // make_interval; the name→kernel mapping is kept for the registry-coverage invariant.
        "make_timestamp" => ScalarFunc::MakeTimestamp,
        "make_timestamptz" => ScalarFunc::MakeTimestamptz,
        "make_date" => ScalarFunc::MakeDate,
        "current_date" => ScalarFunc::CurrentDate,
        "date_part" => ScalarFunc::DatePart,
        // interval normalizers (interval.md §5.1).
        "justify_hours" => ScalarFunc::JustifyHours,
        "justify_days" => ScalarFunc::JustifyDays,
        "justify_interval" => ScalarFunc::JustifyInterval,
        "age" => ScalarFunc::Age,
        // uuid extractors + generators (functions.md §12, entropy.md §3). The generators are
        // volatile (drawn from the entropy seam at eval); the kernel id is still the name.
        "uuid_extract_version" => ScalarFunc::UuidExtractVersion,
        "uuid_extract_timestamp" => ScalarFunc::UuidExtractTimestamp,
        // gen_random_uuid is PostgreSQL's older name for uuidv4 (same kernel, entropy.md §3).
        "uuidv4" | "gen_random_uuid" => ScalarFunc::Uuidv4,
        "uuidv7" => ScalarFunc::Uuidv7,
        "now" => ScalarFunc::Now,
        "clock_timestamp" => ScalarFunc::ClockTimestamp,
        // Sequence value functions (sequences.md §4). nextval/setval MUTATE (write path); all but
        // lastval resolve their text argument to a catalog sequence at eval.
        "nextval" => ScalarFunc::Nextval,
        "currval" => ScalarFunc::Currval,
        "setval" => ScalarFunc::Setval,
        "lastval" => ScalarFunc::Lastval,
        // SessionState-variable read (spec/design/session.md §6.1): reads the session's variable map.
        "current_setting" => ScalarFunc::CurrentSetting,
        // json/jsonb processing functions (B1, json-sql-functions.md §2).
        "jsonb_typeof" => ScalarFunc::JsonbTypeof,
        "json_typeof" => ScalarFunc::JsonTypeof,
        "jsonb_array_length" => ScalarFunc::JsonbArrayLength,
        "json_array_length" => ScalarFunc::JsonArrayLength,
        "jsonb_strip_nulls" => ScalarFunc::JsonbStripNulls,
        "json_strip_nulls" => ScalarFunc::JsonStripNulls,
        "jsonb_pretty" => ScalarFunc::JsonbPretty,
        "json_serialize" => ScalarFunc::JsonSerialize,
        // string / text functions (string-functions.md). char_length/character_length are
        // SQL-standard aliases of length (same code-point-count kernel).
        "length" | "char_length" | "character_length" => ScalarFunc::Length,
        "octet_length" => ScalarFunc::OctetLength,
        "bit_length" => ScalarFunc::BitLength,
        "substr" | "substring" => ScalarFunc::Substr,
        "overlay" => ScalarFunc::Overlay,
        "left" => ScalarFunc::Left,
        "right" => ScalarFunc::Right,
        "lpad" => ScalarFunc::Lpad,
        "rpad" => ScalarFunc::Rpad,
        "btrim" => ScalarFunc::Btrim,
        "ltrim" => ScalarFunc::Ltrim,
        "rtrim" => ScalarFunc::Rtrim,
        "replace" => ScalarFunc::Replace,
        "translate" => ScalarFunc::Translate,
        "repeat" => ScalarFunc::Repeat,
        "reverse" => ScalarFunc::Reverse,
        "strpos" | "position" => ScalarFunc::Strpos,
        "split_part" => ScalarFunc::SplitPart,
        "starts_with" => ScalarFunc::StartsWith,
        "ascii" => ScalarFunc::Ascii,
        "chr" => ScalarFunc::Chr,
        "initcap" => ScalarFunc::Initcap,
        "to_hex" => ScalarFunc::ToHex,
        "encode" => ScalarFunc::Encode,
        "decode" => ScalarFunc::Decode,
        "get_byte" => ScalarFunc::GetByte,
        "set_byte" => ScalarFunc::SetByte,
        "get_bit" => ScalarFunc::GetBit,
        "set_bit" => ScalarFunc::SetBit,
        "md5" => ScalarFunc::Md5,
        "sha224" => ScalarFunc::Sha224,
        "sha256" => ScalarFunc::Sha256,
        "sha384" => ScalarFunc::Sha384,
        "sha512" => ScalarFunc::Sha512,
        "quote_literal" => ScalarFunc::QuoteLiteral,
        "quote_ident" => ScalarFunc::QuoteIdent,
        "quote_nullable" => ScalarFunc::QuoteNullable,
        "like_escape" => ScalarFunc::LikeEscape,
        _ => unreachable!("scalar_func_id: {name} is not a catalog function"),
    }
}

/// The kernel id for VARIADIC function `name` (array-functions.md §12). Total over the catalog's
/// variadic-function names (`is_variadic_func_name` gates the call; `registry_covers_catalog` proves
/// coverage).
pub(crate) fn variadic_func_id(name: &str) -> VariadicFunc {
    match name {
        "num_nulls" => VariadicFunc::NumNulls,
        "num_nonnulls" => VariadicFunc::NumNonnulls,
        "concat" => VariadicFunc::Concat,
        "concat_ws" => VariadicFunc::ConcatWs,
        "format" => VariadicFunc::Format,
        _ => unreachable!("variadic_func_id: {name} is not a catalog variadic function"),
    }
}

/// The result `ScalarType` of a scalar function from its catalog `result` code (functions.md §9):
/// "promoted" = the (single) operand's own type; otherwise the code is a literal scalar-type id
/// (e.g. "decimal", "f64", "interval", "i16", "timestamptz", "uuid") naming the result.
pub(crate) fn scalar_result_type(code: &str, arg_tys: &[ResolvedType]) -> ScalarType {
    if code == "promoted" {
        return resolved_scalar_type(&arg_tys[0]);
    }
    ScalarType::from_name(code)
        .unwrap_or_else(|| unreachable!("scalar_result_type: unknown result code {code}"))
}

/// The concrete `ScalarType` carried by a numeric resolved type (for the "promoted" /
/// "same_as_input" result rules). Only reached for the numeric families those rules admit.
pub(crate) fn resolved_scalar_type(t: &ResolvedType) -> ScalarType {
    match t {
        ResolvedType::Int(it) => *it,
        ResolvedType::Float(ft) => *ft,
        ResolvedType::Decimal => ScalarType::Decimal,
        _ => unreachable!("resolved_scalar_type: non-numeric operand"),
    }
}

/// The `ScalarType` of a *scalar* resolved type, or `None` for a container/null type (composite /
/// array / range / json / null). Total over every `ResolvedType`. Used by the element-wise
/// array→array cast resolver (spec/design/array.md §7) to decide whether the source element type is
/// a scalar with an admitted [`scalar_pair_castable`] cast to the target element scalar.
pub(crate) fn resolved_to_scalar(t: &ResolvedType) -> Option<ScalarType> {
    Some(match t {
        ResolvedType::Int(s) | ResolvedType::Float(s) => *s,
        ResolvedType::Decimal => ScalarType::Decimal,
        ResolvedType::Bool => ScalarType::Bool,
        ResolvedType::Text => ScalarType::Text,
        ResolvedType::Bytea => ScalarType::Bytea,
        ResolvedType::Uuid => ScalarType::Uuid,
        ResolvedType::Timestamp => ScalarType::Timestamp,
        ResolvedType::Timestamptz => ScalarType::Timestamptz,
        ResolvedType::Date => ScalarType::Date,
        ResolvedType::Interval => ScalarType::Interval,
        ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_)
        | ResolvedType::Json
        | ResolvedType::Jsonb
        | ResolvedType::JsonPath
        | ResolvedType::Null => return None,
    })
}

// === Polymorphic array-function resolution (spec/design/array-functions.md §2) ======
// The `anyarray`/`anyelement` pseudo-families are NOT real families (arg_family returns None for
// an array), so the generic `lookup_scalar_overload` cannot match an array function. These helpers
// add the unification: one type variable ELEM, bound from an `anyarray` slot's element type and an
// `anyelement` slot's type, by structural equality (`ResolvedType: Eq`), and read back into the
// reserved result codes `anyarray` (= ELEM[]) and `anyelement` (= ELEM).

/// Whether `name` (exact, grammar.md §3) is a polymorphic array function — a `kind="function"`
/// catalog row whose `arg_families` mention `anyarray`/`anyelement`. Data-driven, so adding an
/// array-function row to the catalog wires it here without touching this gate.
pub(crate) fn is_array_func_name(name: &str) -> bool {
    OPERATORS.iter().any(|o| {
        o.kind == "function"
            && o.name == name
            && o.arg_families
                .iter()
                .any(|f| is_array_slot(f) || is_element_slot(f))
    })
}

/// Whether a catalog slot is an array pseudo-family — strict `anyarray` or PostgreSQL's widening
/// `anycompatiblearray` (array-functions.md §2.1).
pub(crate) fn is_array_slot(slot: &str) -> bool {
    slot == "anyarray" || slot == "anycompatiblearray"
}

/// Whether a catalog slot is an element pseudo-family — strict `anyelement` or the widening
/// `anycompatible` (array-functions.md §2.1).
pub(crate) fn is_element_slot(slot: &str) -> bool {
    slot == "anyelement" || slot == "anycompatible"
}

/// The kernel id for array function `name` (each name is single-arity, so the name alone selects
/// the kernel). Total over the catalog's array-function names (`is_array_func_name` gates the call).
pub(crate) fn array_func_id(name: &str) -> ArrayFunc {
    match name {
        "array_ndims" => ArrayFunc::ArrayNdims,
        "array_length" => ArrayFunc::ArrayLength,
        "array_lower" => ArrayFunc::ArrayLower,
        "array_upper" => ArrayFunc::ArrayUpper,
        "cardinality" => ArrayFunc::Cardinality,
        "array_dims" => ArrayFunc::ArrayDims,
        "array_append" => ArrayFunc::ArrayAppend,
        "array_prepend" => ArrayFunc::ArrayPrepend,
        "array_cat" => ArrayFunc::ArrayCat,
        "array_remove" => ArrayFunc::ArrayRemove,
        "array_replace" => ArrayFunc::ArrayReplace,
        "array_to_string" => ArrayFunc::ArrayToString,
        "string_to_array" => ArrayFunc::StringToArray,
        "array_position" => ArrayFunc::ArrayPosition,
        "array_positions" => ArrayFunc::ArrayPositions,
        _ => unreachable!("array_func_id: {name} is not a catalog array function"),
    }
}

/// Bind/check the type variable ELEM against a concrete type `x`: bind if unbound, else require
/// structural equality — or, for an `anycompatible*` slot (`compatible`), widen ELEM to the common
/// type of the two by the set-operation rule (array-functions.md §2.1, grammar.md §25). `false` ⇒ a
/// conflict (e.g. `array_cat(i32[], text[])`) — the overload does not match. An untyped `NULL`
/// operand never reaches here (the caller defers it).
pub(crate) fn unify_elem(
    elem: &mut Option<ResolvedType>,
    x: &ResolvedType,
    compatible: bool,
) -> bool {
    match elem {
        None => {
            *elem = Some(x.clone());
            true
        }
        Some(e) if compatible => match common_type(e, x) {
            Some(c) => {
                *e = c;
                true
            }
            None => false,
        },
        Some(e) => e == x,
    }
}

/// Match an overload's `arg_families` (which may contain `anyarray`/`anyelement`) against the
/// resolved argument types, returning the bound ELEM (`Some(None)` = matched but every polymorphic
/// arg was an untyped NULL, so ELEM is undeterminable; `None` = no match). Three passes: `anyarray`
/// slots first (they definitively bind ELEM := the element type), then `anyelement` (which may
/// precede its binding array — `array_prepend`), then the concrete family slots.
pub(crate) fn match_poly(slots: &[&str], tys: &[ResolvedType]) -> Option<Option<ResolvedType>> {
    let mut elem: Option<ResolvedType> = None;
    for (slot, t) in std::iter::zip(slots, tys) {
        if is_array_slot(slot) {
            match t {
                ResolvedType::Array(e) => {
                    if !unify_elem(&mut elem, e, *slot == "anycompatiblearray") {
                        return None;
                    }
                }
                ResolvedType::Null => {} // untyped NULL — defer, contributes no binding
                _ => return None,        // a non-array where anyarray is required
            }
        }
    }
    // `anyrange` binds ELEM := the range's element type, like `anyarray` (both definitive, before
    // `anyelement`) — range-functions.md §1.
    for (slot, t) in std::iter::zip(slots, tys) {
        if *slot == "anyrange" {
            match t {
                ResolvedType::Range(e) => {
                    if !unify_elem(&mut elem, e, false) {
                        return None;
                    }
                }
                ResolvedType::Null => {} // untyped NULL — defer, contributes no binding
                _ => return None,        // a non-range where anyrange is required
            }
        }
    }
    for (slot, t) in std::iter::zip(slots, tys) {
        if is_element_slot(slot) {
            match t {
                ResolvedType::Null => {} // untyped NULL — defer
                _ => {
                    if !unify_elem(&mut elem, t, *slot == "anycompatible") {
                        return None;
                    }
                }
            }
        }
    }
    for (slot, t) in std::iter::zip(slots, tys) {
        if !is_array_slot(slot)
            && *slot != "anyrange"
            && !is_element_slot(slot)
            && !family_matches(slot, t)
        {
            return None;
        }
    }
    Some(elem)
}

/// The result `ResolvedType` of an array function from its catalog `result` code and the bound
/// ELEM: `anyarray` → `ELEM[]`, `anyelement` → `ELEM` (both 42P18 if ELEM is undeterminable — every
/// polymorphic arg was an untyped NULL); any other code is a concrete scalar id (`i32`, `text`).
pub(crate) fn poly_result_type(code: &str, elem: &Option<ResolvedType>) -> Result<ResolvedType> {
    match code {
        "anyarray" | "anycompatiblearray" => match elem {
            Some(e) => Ok(ResolvedType::Array(Box::new(e.clone()))),
            None => Err(indeterminate_poly()),
        },
        "anyrange" => match elem {
            Some(e) => Ok(ResolvedType::Range(Box::new(e.clone()))),
            None => Err(indeterminate_poly()),
        },
        "anyelement" => match elem {
            Some(e) => Ok(e.clone()),
            None => Err(indeterminate_poly()),
        },
        // A concrete array result `<scalar>[]` (array_positions → "i32[]"): the element type is
        // fixed (independent of ELEM), so the result is `Array(scalar)` (array-functions.md §8).
        c if c.ends_with("[]") => {
            let base = &c[..c.len() - 2];
            let st = ScalarType::from_name(base)
                .unwrap_or_else(|| unreachable!("poly_result_type: unknown array element {base}"));
            Ok(ResolvedType::Array(Box::new(resolved_type_of(st))))
        }
        _ => Ok(resolved_type_of(
            ScalarType::from_name(code)
                .unwrap_or_else(|| unreachable!("poly_result_type: unknown result code {code}")),
        )),
    }
}

/// Cast one argument of an `anycompatible*` slot to the bound common element type `elem` when its
/// values must change to get there (array-functions.md §2.1): an integer to `decimal`, or an `f32`
/// to `f64` — the element itself (an `anycompatible` slot) or every element of the array (an
/// `anycompatiblearray` slot, through the element-wise array cast). Every other difference the
/// common type admits (integer widths, an anonymous record's integer fields) leaves the values
/// unchanged, so the argument is returned as is, as is an untyped `NULL`.
pub(crate) fn coerce_compatible_arg(
    r: RExpr,
    t: &ResolvedType,
    slot: &str,
    elem: &ResolvedType,
) -> RExpr {
    let from = match (slot, t) {
        ("anycompatible", t) => t,
        ("anycompatiblearray", ResolvedType::Array(e)) => &**e,
        _ => return r,
    };
    let target = match (from, elem) {
        (ResolvedType::Int(_), ResolvedType::Decimal) => ScalarType::Decimal,
        (ResolvedType::Float(ScalarType::Float32), ResolvedType::Float(ScalarType::Float64)) => {
            ScalarType::Float64
        }
        _ => return r,
    };
    if slot == "anycompatible" {
        RExpr::Cast {
            inner: Box::new(r),
            target,
            typmod: None,
            varchar_len: None,
        }
    } else {
        RExpr::ArrayCast {
            inner: Box::new(r),
            to_elem: Some(ColType::Scalar(target)),
        }
    }
}

/// The 42P18 raised when an array function's polymorphic type cannot be determined because every
/// polymorphic argument was an untyped `NULL` (`array_append(NULL, NULL)` — array-functions.md §5).
pub(crate) fn indeterminate_poly() -> EngineError {
    EngineError::new(
        SqlState::IndeterminateDatatype,
        "could not determine polymorphic type because input has type unknown",
    )
}

/// The element type's `ScalarType`, for the literal-adaptation hint (array-functions.md §2): the
/// bound array element type is threaded back as the `ctx` when re-resolving the polymorphic args,
/// so a bare integer/decimal literal element adapts (with range-checking) to that type — e.g.
/// `array_append(i32[], 40)` adapts `40` to `i32`. `None` for a composite/array/NULL element.
pub(crate) fn elem_scalar_hint(t: &ResolvedType) -> Option<ScalarType> {
    match t {
        ResolvedType::Int(s) | ResolvedType::Float(s) => Some(*s),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        ResolvedType::Text => Some(ScalarType::Text),
        ResolvedType::Bool => Some(ScalarType::Bool),
        ResolvedType::Bytea => Some(ScalarType::Bytea),
        ResolvedType::Uuid => Some(ScalarType::Uuid),
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
        ResolvedType::Date => Some(ScalarType::Date),
        ResolvedType::Interval => Some(ScalarType::Interval),
        ResolvedType::Json => Some(ScalarType::Json),
        ResolvedType::JsonPath => Some(ScalarType::JsonPath),
        ResolvedType::Jsonb => Some(ScalarType::Jsonb),
        ResolvedType::Null
        | ResolvedType::Composite(_)
        | ResolvedType::Array(_)
        | ResolvedType::Range(_) => None,
    }
}

/// Resolve a polymorphic array function call (array-functions.md §3): resolve the arguments, unify
/// ELEM across the `anyarray`/`anyelement` slots to pick the overload (42883 on no match), and
/// compute the result type from the matched `result` code. The kernel id is the name; NULL handling
/// (the introspectors propagate, the builders are non-strict) lives in the eval kernel.
///
/// Two passes (§2): pass 1 resolves the arguments with no hint to discover the array's element
/// type; if that element is a scalar, pass 2 re-resolves the polymorphic-slot arguments with it as
/// the `ctx`, so an untyped literal element (or an `ARRAY[…]` constructor argument) adapts to the
/// array's element type — `array_append(i32[], 40)` and `array_cat(i32[], ARRAY[7,8])` both
/// land on `i32`, with a range check on the literal. (The concrete `integer` dimension slot of
/// `array_length`/`lower`/`upper` keeps its pass-1 resolution.)
pub(crate) fn resolve_array_func(
    scope: &Scope,
    name: &str, // the exact (folded) name
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // Each array-function name is single-overload; find its row by (name, arity). A wrong argument
    // count matches no overload (42883), exactly as a missing scalar overload does.
    let desc = OPERATORS
        .iter()
        .find(|o| o.kind == "function" && o.name == name && o.arity as usize == args.len())
        .ok_or_else(|| no_func_overload(name))?;
    let slots = desc.arg_families;

    let mut firsts = Vec::with_capacity(args.len());
    for a in args {
        firsts.push(resolve_first(scope, a, agg, params)?);
    }
    // Pass 2: adapt the polymorphic args to the array's element type, if it is a scalar.
    let hint = slots
        .iter()
        .zip(firsts.iter())
        .find_map(|(slot, f)| match &f.ty {
            ResolvedType::Array(e) if is_array_slot(slot) => elem_scalar_hint(e),
            _ => None,
        });
    let mut rargs = Vec::with_capacity(args.len());
    let mut tys = Vec::with_capacity(args.len());
    for ((a, slot), first) in args.iter().zip(slots.iter()).zip(firsts) {
        let (r, t) = match hint {
            Some(s) if is_array_slot(slot) || is_element_slot(slot) => {
                re_resolve(scope, a, Some(s), first, agg, params)?
            }
            _ => first.into_pair(),
        };
        rargs.push(r);
        tys.push(t);
    }
    let elem = match_poly(slots, &tys).ok_or_else(|| no_func_overload(name))?;
    let result = poly_result_type(desc.result, &elem)?;
    if let Some(e) = &elem {
        rargs = std::iter::zip(rargs, std::iter::zip(slots, &tys))
            .map(|(r, (slot, t))| coerce_compatible_arg(r, t, slot, e))
            .collect();
    }
    Ok((
        RExpr::ArrayFunc {
            func: array_func_id(name),
            args: rargs,
        },
        result,
    ))
}

/// Whether `name` (exact, grammar.md §3) is a polymorphic range function — a `kind="function"` catalog
/// row whose `arg_families` mention `anyrange` (range-functions.md §1). Data-driven, so a new
/// range-function row wires here without touching this gate.
pub(crate) fn is_range_func_name(name: &str) -> bool {
    OPERATORS.iter().any(|o| {
        o.kind == "function" && o.name == name && o.arg_families.iter().any(|f| *f == "anyrange")
    })
}

/// The kernel id for range accessor `name` (each is single-arity, so the name selects the kernel).
/// Total over the catalog's range-function names (`is_range_func_name` gates the call).
pub(crate) fn range_func_id(name: &str) -> RangeFunc {
    match name {
        "lower" => RangeFunc::Lower,
        "upper" => RangeFunc::Upper,
        "isempty" => RangeFunc::IsEmpty,
        "lower_inc" => RangeFunc::LowerInc,
        "upper_inc" => RangeFunc::UpperInc,
        "lower_inf" => RangeFunc::LowerInf,
        "upper_inf" => RangeFunc::UpperInf,
        _ => unreachable!("range_func_id: {name} is not a catalog range function"),
    }
}

/// Resolve a polymorphic range accessor over the `anyrange` pseudo-family (range-functions.md §1).
/// Simpler than [`resolve_array_func`] — the accessors take a single `anyrange` arg with no
/// `anyelement` arg, so there is no element-hint literal adaptation. `lower`/`upper` resolve to ELEM
/// (the bound type), the rest to boolean.
pub(crate) fn resolve_range_func(
    scope: &Scope,
    name: &str, // the exact (folded) name
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let desc = OPERATORS
        .iter()
        .find(|o| o.kind == "function" && o.name == name && o.arity as usize == args.len())
        .ok_or_else(|| no_func_overload(name))?;
    let slots = desc.arg_families;

    let mut rargs = Vec::with_capacity(args.len());
    let mut tys = Vec::with_capacity(args.len());
    for a in args {
        let (r, t) = resolve(scope, a, None, agg, params)?;
        rargs.push(r);
        tys.push(t);
    }
    let elem = match_poly(slots, &tys).ok_or_else(|| no_func_overload(name))?;
    let result = poly_result_type(desc.result, &elem)?;
    // `range_merge(anyrange, anyrange) → anyrange` is a SET operation (= union, non-strict), not a
    // scalar accessor: emit the shared `RangeSetOp` node (range-functions.md §4). `poly_result_type`
    // already raised 42P18 if the element was indeterminate (both args untyped NULL), so `elem` is
    // bound here.
    if name == "range_merge" {
        return Ok((
            RExpr::RangeSetOp {
                op: RangeSetOp::Merge,
                args: rargs,
            },
            result,
        ));
    }
    Ok((
        RExpr::RangeFunc {
            func: range_func_id(name),
            args: rargs,
        },
        result,
    ))
}

/// Resolve `lower`/`upper`, overloaded across the range accessors and the text casing functions
/// (functions.md §9, collation.md §16). The single argument resolves once (offering `text` as the
/// literal-adaptation hint, so a bare NULL / untyped `$1` adapts to text — the common case; a typed
/// range expression keeps its range type and ignores the scalar hint). A **text/NULL** argument folds
/// case (`RExpr::Casing`, result `text`); a **range** argument is the bound accessor (`RExpr::RangeFunc`,
/// result the range's element type); anything else is `42883` (no overload).
pub(crate) fn resolve_lower_upper(
    scope: &Scope,
    name: &str, // "lower" | "upper", the exact (folded) name
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.len() != 1 {
        return Err(no_func_overload(name));
    }
    let (r, t) = resolve(scope, &args[0], Some(ScalarType::Text), agg, params)?;
    match t {
        ResolvedType::Text | ResolvedType::Null => Ok((
            RExpr::Casing {
                upper: name == "upper",
                arg: Box::new(r),
            },
            ResolvedType::Text,
        )),
        ResolvedType::Range(elem) => Ok((
            RExpr::RangeFunc {
                func: range_func_id(name),
                args: vec![r],
            },
            *elem, // lower(anyrange)/upper(anyrange) return the element type
        )),
        _ => Err(no_func_overload(name)),
    }
}

/// Resolve `timezone(zone, value)` — the desugar of `value AT TIME ZONE zone` (timezones.md §6).
/// `zone` must be text (else `42804`); the result family is the OTHER timestamp family of `value`:
/// `timestamptz` → `timestamp` (render the instant locally) and `timestamp` → `timestamptz`
/// (interpret the wall clock in the zone). Any other `value` family — or an untyped/NULL value, which
/// cannot pick an overload — is `42883`.
pub(crate) fn resolve_timezone(
    scope: &Scope,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.len() != 2 {
        return Err(no_func_overload("timezone"));
    }
    // args[0] = zone (text), args[1] = value (timestamp/timestamptz). The AT TIME ZONE desugar puts
    // the zone first, matching PostgreSQL's `timezone(text, timestamptz)` signature.
    let (zone_r, zone_t) = resolve(scope, &args[0], Some(ScalarType::Text), agg, params)?;
    let (value_r, value_t) = resolve(scope, &args[1], None, agg, params)?;
    // A non-text zone, or a non-timestamp value, is `42883` — PG resolves AT TIME ZONE via function
    // overload (`timezone(text, timestamptz)` / `timezone(text, timestamp)`), so any other arg pair
    // is "no such function" (PG-matching, oracle-pinned), not a datatype_mismatch. A NULL zone is
    // allowed (it propagates to NULL at eval).
    let zone_ok = matches!(zone_t, ResolvedType::Text | ResolvedType::Null);
    let (to_timestamptz, result) = match (zone_ok, value_t) {
        (true, ResolvedType::Timestamptz) => (false, ResolvedType::Timestamp),
        (true, ResolvedType::Timestamp) => (true, ResolvedType::Timestamptz),
        _ => return Err(no_func_overload("timezone")),
    };
    params.note_index_zone(&zone_r, &scope.catalog.session.env)?;
    Ok((
        RExpr::AtTimeZone {
            zone: Box::new(zone_r),
            value: Box::new(value_r),
            to_timestamptz,
        },
        result,
    ))
}

/// Resolve `date_trunc(unit, value[, zone])` (timezones.md §9.1). `unit` is text (a runtime value,
/// validated at eval); `value` is `timestamp` / `timestamptz` / `interval`; the optional `zone` (text)
/// is the 3-arg form, valid **only** for a `timestamptz` value. The result family is the `value`
/// family. A non-text unit/zone, a non-datetime value, or the 3-arg form on a non-`timestamptz` value
/// is `42883` (no such overload — PG-matching; a `date` value also has no overload, jed having no
/// implicit `date`→`timestamp` cast).
pub(crate) fn resolve_date_trunc(
    scope: &Scope,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if args.len() != 2 && args.len() != 3 {
        return Err(no_func_overload("date_trunc"));
    }
    let (unit_r, unit_t) = resolve(scope, &args[0], Some(ScalarType::Text), agg, params)?;
    let (value_r, value_t) = resolve(scope, &args[1], None, agg, params)?;
    if !matches!(unit_t, ResolvedType::Text | ResolvedType::Null) {
        return Err(no_func_overload("date_trunc"));
    }
    let result = match value_t {
        ResolvedType::Timestamp | ResolvedType::Timestamptz | ResolvedType::Interval => value_t,
        _ => return Err(no_func_overload("date_trunc")),
    };
    let zone = if args.len() == 3 {
        // The 3-arg form is `date_trunc(text, timestamptz, text)` only (PG): a 3-arg call on a
        // timestamp/interval value is "no such function".
        if !matches!(result, ResolvedType::Timestamptz) {
            return Err(no_func_overload("date_trunc"));
        }
        let (zone_r, zone_t) = resolve(scope, &args[2], Some(ScalarType::Text), agg, params)?;
        if !matches!(zone_t, ResolvedType::Text | ResolvedType::Null) {
            return Err(no_func_overload("date_trunc"));
        }
        Some(Box::new(zone_r))
    } else {
        None
    };
    if let Some(z) = &zone {
        params.note_index_zone(z, &scope.catalog.session.env)?;
    } else {
        params.nonimmutable |= matches!(result, ResolvedType::Timestamptz);
    }
    Ok((
        RExpr::DateTrunc {
            unit: Box::new(unit_r),
            value: Box::new(value_r),
            zone,
        },
        result,
    ))
}

/// Whether `name` (exact, grammar.md §3) is a range CONSTRUCTOR call (range-functions.md §2): a call
/// whose name is a range type name or alias (`i32range`/`int4range`/`numrange`/…). The constructor
/// functions are the only ones whose name is a range type name, so `range::range_by_name` resolving
/// is exactly the gate — data-driven over the RANGES table, no hand-written name list.
pub(crate) fn is_range_ctor_name(name: &str) -> bool {
    crate::range::range_by_name(name).is_some()
}

/// Whether a bound argument of resolved type `t` is assignable to range element `elem`, mirroring
/// the `store_value` coercions the kernel will apply (range-functions.md §2): a NULL is an infinite
/// bound (always ok); an integer adapts to an integer (range-checked) or decimal element; a decimal
/// to a decimal element; an already-temporal value to its own element; and a string literal/text to
/// a temporal element (parsed at eval). Anything else is no overload (42883).
pub(crate) fn range_bound_assignable(t: &ResolvedType, elem: ScalarType) -> bool {
    match t {
        ResolvedType::Null => true,
        ResolvedType::Int(_) => elem.is_integer() || elem.is_decimal(),
        ResolvedType::Decimal => elem.is_decimal(),
        ResolvedType::Timestamp => elem.is_timestamp(),
        ResolvedType::Timestamptz => elem.is_timestamptz(),
        ResolvedType::Date => elem.is_date(),
        ResolvedType::Text => elem.is_timestamp() || elem.is_timestamptz() || elem.is_date(),
        _ => false,
    }
}

/// Resolve a range constructor call (`i32range(lo, hi[, bounds])` and the five siblings, plus the
/// `int4range`/`int8range` aliases — range-functions.md §2). The target range type comes from the
/// call name (`range_by_name`, alias-aware); the result type is fixed (concrete), not polymorphic.
/// Each bound resolves with the element scalar as the literal-adaptation context (so `1` adapts to
/// the element width, `'2024-01-01'` to a date), then is type-checked assignable to the element; the
/// optional third argument is the bounds-flags TEXT. The kernel ([`eval_range_ctor`]) does the
/// element coercion (assignment-style, 22003), the flags parse (42601 / 22000), and `finalize`.
pub(crate) fn resolve_range_ctor(
    scope: &Scope,
    name: &str, // the exact (folded) name
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let desc = crate::range::range_by_name(name).expect("is_range_ctor_name gated the call");
    let elem = crate::range::element_scalar(desc);
    // Only the 2-arg (lo, hi) and 3-arg (lo, hi, bounds) overloads exist.
    if args.len() != 2 && args.len() != 3 {
        return Err(no_func_overload(name));
    }
    let mut rargs = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        if i < 2 {
            // A bound: offer the element scalar as the literal-adaptation hint, then check the
            // resolved type is assignable to the element (else no overload).
            let (r, t) = resolve(scope, a, Some(elem), agg, params)?;
            if !range_bound_assignable(&t, elem) {
                return Err(no_func_overload(name));
            }
            rargs.push(r);
        } else {
            // The bounds-flags argument: TEXT (a NULL is allowed at resolve — the kernel traps it
            // 22000 at eval, matching PG "flags argument must not be null").
            let (r, t) = resolve(scope, a, None, agg, params)?;
            if !matches!(t, ResolvedType::Text | ResolvedType::Null) {
                return Err(no_func_overload(name));
            }
            rargs.push(r);
        }
    }
    Ok((
        RExpr::RangeCtor { elem, args: rargs },
        ResolvedType::Range(Box::new(resolved_type_of(elem))),
    ))
}

/// Whether a function NAME (already folded / exact, grammar.md §3) names the catalog `surface`
/// spelling. Catalog surfaces are a fixed vocabulary authored in uppercase (`COUNT`), so the name
/// matches the surface's lowercase spelling byte-exactly — `count` resolves, a quoted `"COUNT"`
/// does not (PostgreSQL's rule).
pub(crate) fn surface_matches(surface: &str, name: &str) -> bool {
    surface.len() == name.len()
        && surface
            .bytes()
            .zip(name.bytes())
            .all(|(s, n)| s.to_ascii_lowercase() == n)
}

/// Whether aggregate `surface` (lowercase function name) has a `COUNT(*)`-style star overload — only
/// COUNT does. The data-driven replacement for the special-cased `_ if star` arm.
pub(crate) fn aggregate_has_star(surface: &str) -> bool {
    AGGREGATES
        .iter()
        .any(|a| surface_matches(a.surface, surface) && a.arg == "star")
}

/// The matched aggregate overload row for `surface` over a single operand of resolved type `t`:
/// the `arg="expr"` catalog row whose lone `arg_families` slot matches. `None` ⇒ no overload
/// (42883, e.g. `SUM(text)`). MIN/MAX/COUNT take "any" (NULL included); SUM/AVG a numeric family.
pub(crate) fn lookup_aggregate_overload(
    surface: &str,
    t: &ResolvedType,
) -> Option<&'static AggregateDesc> {
    AGGREGATES.iter().find(|a| {
        surface_matches(a.surface, surface)
            && a.arg == "expr"
            && a.arg_families.len() == 1
            && family_matches(a.arg_families[0], t)
    })
}

/// Resolve the arguments of a multi-argument aggregate call in `sub` (aggregates are forbidden in
/// them) and select its catalog overload (aggregates.md §23). The overload must match every
/// argument's family; failing that, an untyped NULL or a bare string literal is unknown-typed and
/// adopts its slot's family (the scalar functions' rule, functions.md §9.1): `string_agg(b, ',')`
/// reads `','` as bytea when `b` is. An unknown argument is then re-resolved with that family's type.
pub(crate) fn resolve_agg_args(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    sub: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(Vec<RExpr>, Vec<ResolvedType>, &'static AggregateDesc)> {
    let mut rargs = Vec::with_capacity(args.len());
    let mut tys = Vec::with_capacity(args.len());
    for a in args {
        if expr_has_window(a) {
            return Err(EngineError::new(
                SqlState::GroupingError,
                "aggregate function calls cannot contain window function calls",
            ));
        }
        let (r, t) = resolve(scope, a, None, sub, params)?;
        rargs.push(r);
        tys.push(t);
    }
    let literals = bare_string_literals(args);
    let mut desc = select_aggregate_overload(name, &tys, None)?;
    if desc.is_none() && literals.is_some() {
        desc = select_aggregate_overload(name, &tys, literals.as_deref())?;
    }
    let desc = desc.ok_or_else(|| no_agg_overload(name))?;
    for (i, t) in tys.iter_mut().enumerate() {
        let literal = literals.as_ref().is_some_and(|l| l[i]);
        if !matches!(t, ResolvedType::Null) && !literal {
            continue;
        }
        let Some(st) = family_param_type(desc.arg_families[i]) else {
            continue;
        };
        if literal || matches!(args[i], Expr::Param(_)) {
            let (r, rt) = resolve(scope, &args[i], Some(st), sub, params)?;
            rargs[i] = r;
            *t = rt;
        } else {
            *t = resolved_type_of(st);
        }
    }
    Ok((rargs, tys, desc))
}

/// Pick a multi-argument aggregate's overload. With `literals` `None` only an untyped NULL is
/// unknown (it matches any slot); otherwise a marked bare string literal is unknown too, matching
/// the slots [`literal_adapts`] admits. Several candidates are narrowed as for a scalar function: at
/// each unknown position text wins, else the family every candidate shares; any other disagreement
/// is 42725.
pub(crate) fn select_aggregate_overload(
    name: &str,
    tys: &[ResolvedType],
    literals: Option<&[bool]>,
) -> Result<Option<&'static AggregateDesc>> {
    let unknown = |i: usize| matches!(tys[i], ResolvedType::Null) || literals.is_some_and(|l| l[i]);
    let candidates: Vec<&'static AggregateDesc> = AGGREGATES
        .iter()
        .filter(|a| {
            surface_matches(a.surface, name)
                && a.arg == "expr"
                && a.arg_families.len() == tys.len()
                && a.arg_families.iter().enumerate().all(|(j, slot)| {
                    if literals.is_some_and(|l| l[j]) {
                        literal_adapts(slot)
                    } else {
                        matches!(tys[j], ResolvedType::Null) || family_matches(slot, &tys[j])
                    }
                })
        })
        .collect();
    if candidates.len() <= 1 {
        return Ok(candidates.first().copied());
    }
    let mut kept = candidates;
    for i in (0..tys.len()).filter(|&i| unknown(i)) {
        if kept.iter().any(|a| a.arg_families[i] == "text") {
            kept.retain(|a| a.arg_families[i] == "text");
        } else if kept
            .iter()
            .any(|a| a.arg_families[i] != kept[0].arg_families[i])
        {
            return Err(ambiguous_func_overload(name));
        }
    }
    if kept.len() != 1 {
        return Err(ambiguous_func_overload(name));
    }
    Ok(Some(kept[0]))
}

/// Reject an untyped NULL operand of an aggregate whose result type is derived from it (array_agg —
/// aggregates.md §23). PostgreSQL's 42725: array_agg has an element and an array overload, and an
/// unknown argument matches both.
pub(crate) fn check_polymorphic_agg_operand(desc: &AggregateDesc, t: &ResolvedType) -> Result<()> {
    if desc.result == "array_of_input" && matches!(t, ResolvedType::Null) {
        return Err(ambiguous_func_overload(&desc.surface.to_ascii_lowercase()));
    }
    Ok(())
}

/// The runtime plan + result type for an aggregate over operand type `t`, from the matched
/// overload's `surface` + catalog `result` code (the PG widening — aggregates.md §3). The plan
/// is the aggregate's kernel id (fold/finalize switch on it); selecting it from the registered
/// `result` code keeps the name gate + overload validation data-driven while the kernel stays
/// hand-written (§5). `surface` is the (folded) call name; `result` the matched row's code.
pub(crate) fn aggregate_plan(
    surface: &str,
    result: &str,
    t: &ResolvedType,
) -> (AggPlan, ResolvedType) {
    match (surface, result) {
        ("count", _) => (AggPlan::Count, ResolvedType::Int(ScalarType::Int64)),
        // SUM(i16|i32) → i64; SUM(i64) → decimal (PG widening).
        ("sum", "sum_widen") => match t {
            ResolvedType::Int(it) if *it == ScalarType::Int64 => {
                (AggPlan::SumDecimal, ResolvedType::Decimal)
            }
            ResolvedType::Int(_) => (AggPlan::SumInt, ResolvedType::Int(ScalarType::Int64)),
            _ => unreachable!("sum_widen matches only the integer family"),
        },
        ("sum", "decimal") => (AggPlan::SumDecimal, ResolvedType::Decimal),
        // SUM(float)/AVG(float) → SAME width (the canonical-order fold — float.md §7).
        ("sum", "same_as_input") => {
            let ft = resolved_scalar_type(t);
            (AggPlan::SumFloat(ft), ResolvedType::Float(ft))
        }
        ("avg", "decimal") => (AggPlan::Avg, ResolvedType::Decimal),
        ("avg", "same_as_input") => {
            let ft = resolved_scalar_type(t);
            (AggPlan::AvgFloat(ft), ResolvedType::Float(ft))
        }
        // MIN/MAX accept any ordered scalar; the result is the argument's own type.
        ("min", "same_as_input") => (AggPlan::Min, t.clone()),
        ("max", "same_as_input") => (AggPlan::Max, t.clone()),
        ("bool_and" | "every", "boolean") => (AggPlan::BoolAnd, ResolvedType::Bool),
        ("bool_or", "boolean") => (AggPlan::BoolOr, ResolvedType::Bool),
        ("bit_and", "same_as_input") => (AggPlan::Bit(BitOp::And), t.clone()),
        ("bit_or", "same_as_input") => (AggPlan::Bit(BitOp::Or), t.clone()),
        ("bit_xor", "same_as_input") => (AggPlan::Bit(BitOp::Xor), t.clone()),
        ("regr_count", "i64") => (AggPlan::RegrCount, ResolvedType::Int(ScalarType::Int64)),
        // The two-argument statistical aggregates: numeric arguments widened to f64.
        (s, "f64") if PairStat::from_surface(s).is_some() => (
            AggPlan::Pair {
                stat: PairStat::from_surface(s).unwrap(),
            },
            ResolvedType::Float(ScalarType::Float64),
        ),
        // The statistical aggregates: exact over integer/decimal (a decimal result), float over
        // float.
        (s, "decimal" | "f64") if MomentStat::from_surface(s).is_some() => {
            let stat = MomentStat::from_surface(s).unwrap();
            if result == "f64" {
                (
                    AggPlan::Moment { stat, float: true },
                    ResolvedType::Float(ScalarType::Float64),
                )
            } else {
                (
                    AggPlan::Moment { stat, float: false },
                    ResolvedType::Decimal,
                )
            }
        }
        ("string_agg", "text") => (AggPlan::StringAgg, ResolvedType::Text),
        ("string_agg", "bytea") => (AggPlan::StringAgg, ResolvedType::Bytea),
        // An array input stacks into one more dimension of the same (dimension-agnostic) array type.
        ("array_agg", "array_of_input") => match t {
            ResolvedType::Array(_) => (AggPlan::ArrayAgg { nested: true }, t.clone()),
            _ => (
                AggPlan::ArrayAgg { nested: false },
                ResolvedType::Array(Box::new(t.clone())),
            ),
        },
        // json/jsonb array aggregates (B4). `compact` is the json (vs jsonb) render; `_strict`
        // skips a NULL input. The result type is json/jsonb (the catalog `result` code).
        ("jsonb_agg", "jsonb") => (
            AggPlan::JsonAgg {
                compact: false,
                strict: false,
            },
            ResolvedType::Jsonb,
        ),
        ("json_agg", "json") => (
            AggPlan::JsonAgg {
                compact: true,
                strict: false,
            },
            ResolvedType::Json,
        ),
        ("jsonb_agg_strict", "jsonb") => (
            AggPlan::JsonAgg {
                compact: false,
                strict: true,
            },
            ResolvedType::Jsonb,
        ),
        ("json_agg_strict", "json") => (
            AggPlan::JsonAgg {
                compact: true,
                strict: true,
            },
            ResolvedType::Json,
        ),
        _ => unreachable!("aggregate_plan: unhandled ({surface}, {result})"),
    }
}

/// Resolve an aggregate call into a synthetic-row reference, collecting its `AggSpec`. Only
/// valid in `Collect` mode; in `Forbidden` mode (WHERE/ON/nested) it is 42803. The operand is
/// resolved in a fresh `Forbidden` sub-context (a nested aggregate is 42803; its columns
/// resolve against the real row). The result type follows the PG widening (aggregates.md §3).
pub(crate) fn resolve_aggregate(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    star: bool,
    distinct: bool,
    filter: Option<&Expr>,
    agg_order: Option<&[OrderKey]>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if !matches!(agg, AggCtx::Collect { .. } | AggCtx::GroupedWindow { .. }) {
        // Forbidden (WHERE/JOIN ON/plain projection) and Window (a plain window query's projection)
        // both reject a bare aggregate here — 42803.
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate functions are not allowed here",
        ));
    }
    let mut sub = AggCtx::Forbidden;
    // json[b]_object_agg[_unique] take TWO operands (key, value) — resolve both and encode as a Row
    // operand for the single-operand aggregate framework (the fold splits the composite back out).
    if let Some((json, unique)) = object_agg_classify(name) {
        if star || args.len() != 2 {
            return Err(no_agg_overload(name));
        }
        let (rk, kt) = resolve(scope, &args[0], None, &mut sub, params)?;
        let (rv, vt) = resolve(scope, &args[1], None, &mut sub, params)?;
        // DISTINCT de-duplicates the (key, value) pair, so both need an equality (json has none).
        if distinct {
            reject_json_distinct(&kt)?;
            reject_json_distinct(&vt)?;
        }
        let order = match agg_order {
            Some(keys) => Some(resolve_agg_order(
                scope,
                keys,
                distinct,
                args,
                &[&rk, &rv],
                params,
            )?),
            None => None,
        };
        let filter = resolve_agg_filter(scope, filter, params)?;
        let operand = RExpr::Row(vec![rk, rv]);
        let plan = AggPlan::JsonObjectAgg { json, unique };
        let result = if json {
            ResolvedType::Json
        } else {
            ResolvedType::Jsonb
        };
        // The key/value pair travels as one Row operand; DISTINCT dedups that pair, FILTER and the
        // in-aggregate ORDER BY apply as for any aggregate. It collects into `Collect` or
        // `GroupedWindow` (a grouped query that also windows), exactly like any aggregate
        // (spec/design/window.md §5.1).
        return match agg {
            AggCtx::Collect {
                group_keys, specs, ..
            } => {
                let slot = group_keys.len() + specs.len();
                specs.push(AggSpec {
                    plan,
                    operand: Some(operand),
                    distinct,
                    filter,
                    osa: None,
                    hypo: None,
                    order,
                    collation: None,
                    json_tys: vec![kt.clone(), vt.clone()],
                });
                Ok((RExpr::Column(slot), result))
            }
            AggCtx::GroupedWindow {
                group_keys,
                agg_specs,
                ..
            } => {
                let slot = group_keys.len() + agg_specs.len();
                agg_specs.push(AggSpec {
                    plan,
                    operand: Some(operand),
                    distinct,
                    filter,
                    osa: None,
                    hypo: None,
                    order,
                    collation: None,
                    json_tys: vec![kt.clone(), vt.clone()],
                });
                Ok((RExpr::Column(slot), result))
            }
            _ => unreachable!("an aggregate in a non-collecting context is handled above"),
        };
    }
    let (plan, operand, result, json_tys) = if star {
        // Only COUNT has a star overload (aggregates.md §3); `SUM(*)` etc. is a syntax error.
        if !aggregate_has_star(name) {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        (
            AggPlan::CountStar,
            None,
            ResolvedType::Int(ScalarType::Int64),
            Vec::new(),
        )
    } else if args.len() >= 2 {
        // A multi-argument aggregate (string_agg — aggregates.md §23): its arguments travel as one
        // Row operand, which the fold splits back out.
        let (rargs, tys, desc) = resolve_agg_args(scope, name, args, &mut sub, params)?;
        let (plan, result) = aggregate_plan(name, desc.result, &tys[0]);
        if distinct {
            for t in &tys {
                reject_json_distinct(t)?;
            }
        }
        (plan, Some(RExpr::Row(rargs)), result, Vec::new())
    } else {
        // One operand, resolved in a fresh Forbidden sub-context. The registry validates the
        // (surface, operand-family) overload exists (else 42883) and yields its result code; the
        // plan + result type follow from it (the PG widening).
        let arg = expect_arg(args)?;
        // An aggregate's argument may not contain a window function (PG 42803 — window.md §7): the
        // window stage runs AFTER aggregation, so a window result cannot be folded into an aggregate.
        if expr_has_window(arg) {
            return Err(EngineError::new(
                SqlState::GroupingError,
                "aggregate function calls cannot contain window function calls",
            ));
        }
        let (r, t) = resolve(scope, arg, None, &mut sub, params)?;
        let desc = lookup_aggregate_overload(name, &t).ok_or_else(|| no_agg_overload(name))?;
        check_polymorphic_agg_operand(desc, &t)?;
        let (plan, result) = aggregate_plan(name, desc.result, &t);
        if distinct {
            reject_json_distinct(&t)?;
        }
        // A JSON aggregate converts each input by its static type (json-sql-functions.md §4).
        let json_tys = if matches!(plan, AggPlan::JsonAgg { .. }) {
            vec![t]
        } else {
            Vec::new()
        };
        (plan, Some(r), result, json_tys)
    };
    // The in-aggregate ORDER BY (aggregates.md §22) — resolved after the overload, like PG.
    let order = match agg_order {
        Some(keys) => {
            // A multi-argument aggregate's argument nodes are its Row operand's fields.
            let arg_nodes: Vec<&RExpr> = match &operand {
                Some(RExpr::Row(fields)) if args.len() >= 2 => fields.iter().collect(),
                _ => operand.iter().collect(),
            };
            Some(resolve_agg_order(
                scope, keys, distinct, args, &arg_nodes, params,
            )?)
        }
        None => None,
    };
    let collation = minmax_collation(scope, plan, args, &result)?;
    // FILTER (WHERE cond): resolve the per-row predicate against the input row with aggregates
    // FORBIDDEN — an aggregate inside FILTER is 42803, matching PG (aggregates.md §11). A
    // non-boolean condition (or an untyped NULL, always unknown → folds no row) is 42804. The
    // fold loop evaluates this per row and folds only the rows for which it is TRUE.
    let rfilter = resolve_agg_filter(scope, filter, params)?;
    // Aggregate results follow the group-key values in the synthetic row. A grouped+window query
    // (`GroupedWindow`) collects into the SAME `agg_specs` (its window results are slotted after
    // every aggregate — spec/design/window.md §5.1).
    match agg {
        AggCtx::Collect {
            group_keys, specs, ..
        } => {
            let slot = group_keys.len() + specs.len();
            specs.push(AggSpec {
                plan,
                operand,
                distinct,
                filter: rfilter,
                osa: None,
                hypo: None,
                order,
                collation: collation.clone(),
                json_tys: json_tys.clone(),
            });
            Ok((RExpr::Column(slot), result))
        }
        AggCtx::GroupedWindow {
            group_keys,
            agg_specs,
            ..
        } => {
            let slot = group_keys.len() + agg_specs.len();
            agg_specs.push(AggSpec {
                plan,
                operand,
                distinct,
                filter: rfilter,
                osa: None,
                hypo: None,
                order,
                collation: collation.clone(),
                json_tys: json_tys.clone(),
            });
            Ok((RExpr::Column(slot), result))
        }
        _ => unreachable!("an aggregate in a non-collecting context is handled above"),
    }
}

/// DISTINCT de-duplicates an aggregate's argument by equality, and `json` has none (PG ships no
/// equality opclass — json.md §5): a DISTINCT aggregate over a json argument is 42883, the same
/// rule `SELECT DISTINCT` applies to a json output column.
pub(crate) fn reject_json_distinct(t: &ResolvedType) -> Result<()> {
    if matches!(t, ResolvedType::Json) {
        return Err(EngineError::new(
            SqlState::UndefinedFunction,
            "could not identify an equality operator for type json",
        )
        .no_hint());
    }
    Ok(())
}

/// Resolve an aggregate's `FILTER (WHERE cond)` (aggregates.md §11): a per-row predicate over the
/// input row with aggregates FORBIDDEN (an aggregate inside FILTER is 42803); a non-boolean
/// condition (an untyped NULL is allowed — always unknown, folds no row) is 42804.
pub(crate) fn resolve_agg_filter(
    scope: &Scope,
    filter: Option<&Expr>,
    params: &mut ParamTypes,
) -> Result<Option<RExpr>> {
    let Some(f) = filter else { return Ok(None) };
    let mut fsub = AggCtx::Forbidden;
    let (rf, ft) = resolve(scope, f, None, &mut fsub, params)?;
    match ft {
        ResolvedType::Bool | ResolvedType::Null => Ok(Some(rf)),
        _ => Err(EngineError::new(
            SqlState::DatatypeMismatch,
            "argument of FILTER must be type boolean",
        )
        .with_position(f.location().0)),
    }
}

/// The expression an aggregate sort key orders by: its general expression, or the (qualified)
/// column reference a column key names.
pub(crate) fn agg_sort_key_expr(key: &OrderKey) -> Expr {
    match &key.expr {
        Some(e) => e.clone(),
        None => match &key.qualifier {
            Some(q) => Expr::QualifiedColumn {
                qualifier: q.clone(),
                name: key.column.clone(),
                pos: key.pos,
            },
            None => Expr::Column(key.column.clone(), key.pos),
        },
    }
}

/// Resolve one aggregate sort key — a `WITHIN GROUP` key of a hypothetical-set aggregate
/// (aggregates.md §19) or an in-aggregate `ORDER BY` key (§22) — against the input row, aggregates
/// forbidden (a nested aggregate is 42803). Returns the per-row key expression, its type, and its
/// sort spec: direction, NULL placement, and collation (an explicit COLLATE — text only, else 42804
/// — or the key's derived collation, §13).
pub(crate) fn resolve_agg_sort_key(
    scope: &Scope,
    key: &OrderKey,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType, KeySort)> {
    let mut sub = AggCtx::Forbidden;
    let (knode, ktype) = resolve(scope, &agg_sort_key_expr(key), None, &mut sub, params)?;
    let collation: Option<std::sync::Arc<Collation>> = match &key.collation {
        Some(cn) => {
            if !ktype.collatable() {
                return Err(type_error(format!(
                    "collations are not supported by type {}",
                    ktype.type_name()
                )));
            }
            resolve_collation_name(scope.catalog, cn)?
        }
        None if ktype.collatable() => {
            sort_target_collation(scope.catalog, order_key_deriv(scope, key)?)?
        }
        None => None,
    };
    let sort = KeySort {
        desc: key.descending,
        nulls_first: key.nulls_first,
        collation,
    };
    Ok((knode, ktype, sort))
}

/// Resolve an in-aggregate `ORDER BY` (`json_agg(x ORDER BY y DESC)` — aggregates.md §22): each key
/// is evaluated per passing input row and the group's rows are folded in key order. A `json` key is
/// 42883 (no ordering operator, like the query ORDER BY). With DISTINCT every key must be one of the
/// aggregate's arguments — structurally (the AST, positions ignored) or as the same input column —
/// else 42P10, PostgreSQL's rule (the sort then never splits a de-duplicated value).
pub(crate) fn resolve_agg_order(
    scope: &Scope,
    keys: &[OrderKey],
    distinct: bool,
    args: &[Expr],
    arg_nodes: &[&RExpr],
    params: &mut ParamTypes,
) -> Result<AggOrder> {
    let mut rkeys = Vec::with_capacity(keys.len());
    let mut sorts = Vec::with_capacity(keys.len());
    for key in keys {
        // Like an argument, a key may not contain a window function (PG 42803 — window.md §7).
        if key.expr.as_ref().is_some_and(expr_has_window) {
            return Err(EngineError::new(
                SqlState::GroupingError,
                "aggregate function calls cannot contain window function calls",
            ));
        }
        let (knode, ktype, sort) = resolve_agg_sort_key(scope, key, params)?;
        if matches!(ktype, ResolvedType::Json) {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                "could not identify an ordering operator for type json",
            )
            .hinted(error_hint::NO_ORDERING_OPERATOR));
        }
        if distinct {
            let kexpr = agg_sort_key_expr(key);
            let same_column = |a: &&RExpr| match (a, &knode) {
                (RExpr::Column(i), RExpr::Column(j)) => i == j,
                _ => false,
            };
            if !args.iter().any(|a| *a == kexpr) && !arg_nodes.iter().any(same_column) {
                return Err(EngineError::new(
                    SqlState::InvalidColumnReference,
                    "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list",
                ));
            }
        }
        rkeys.push(knode);
        sorts.push(sort);
    }
    Ok(AggOrder { keys: rkeys, sorts })
}

/// The comparison collation of a text / text[] `MIN`/`MAX` (spec/design/collation.md §1 "Aggregates and
/// window functions"): the operand's own derivation, so a conflicted or indeterminate operand is
/// 42P22 (PostgreSQL raises it once two values compare). `None` for any other aggregate.
pub(crate) fn minmax_collation(
    scope: &Scope,
    plan: AggPlan,
    args: &[Expr],
    result: &ResolvedType,
) -> Result<Option<std::sync::Arc<Collation>>> {
    if !matches!(plan, AggPlan::Min | AggPlan::Max) || !result.collatable() {
        return Ok(None);
    }
    match args.first() {
        Some(arg) => resolve_deriv(scope.catalog, derive_collation(scope, arg)?),
        None => Ok(None),
    }
}

/// Resolve an ordered-set aggregate `agg(direct_args) WITHIN GROUP (ORDER BY key)` — `mode`,
/// `percentile_cont`, `percentile_disc` (spec/design/aggregates.md §13). Like `resolve_aggregate`
/// it is valid only in a collecting context (else 42803) and folds into the same `AggSpec` list,
/// returning a synthetic-row reference. The `WITHIN GROUP` key is the aggregate's operand
/// (resolved with aggregates forbidden — a nested aggregate is 42803); the parenthesized `args`
/// are the per-group direct argument (the percentile fraction; empty for `mode`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_ordered_set_aggregate(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    keys: &[OrderKey],
    distinct: bool,
    filter: Option<&Expr>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if !matches!(agg, AggCtx::Collect { .. } | AggCtx::GroupedWindow { .. }) {
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate functions are not allowed here",
        ));
    }
    // DISTINCT cannot decorate an ordered-set aggregate (PG: a 42601 syntax error).
    if distinct {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "DISTINCT is not allowed with ordered-set aggregates",
        ));
    }
    // Exactly one WITHIN GROUP sort key (PG models a second as a missing overload → 42883).
    let [key] = keys else {
        return Err(no_agg_overload(name));
    };
    // The aggregated argument: the WITHIN GROUP order key, resolved per row with aggregates FORBIDDEN
    // (a nested aggregate in the order key is 42803, matching PG). A general-expression key
    // (`ORDER BY a + b`) carries a resolved `expr`; a bare/qualified column key carries `column`
    // (rebuilt here as an `Expr` so both paths share one resolve).
    let mut sub = AggCtx::Forbidden;
    let (operand, optype) = match &key.expr {
        Some(e) => resolve(scope, e, None, &mut sub, params)?,
        None => {
            let key_expr = match &key.qualifier {
                Some(q) => Expr::QualifiedColumn {
                    qualifier: q.clone(),
                    name: key.column.clone(),
                    pos: key.pos,
                },
                None => Expr::Column(key.column.clone(), key.pos),
            };
            resolve(scope, &key_expr, None, &mut sub, params)?
        }
    };
    // The WITHIN GROUP key's COLLATION drives the sort (aggregates.md §13): an explicit `COLLATE`
    // on the key (text operand only — else "collations are not supported by type T", like the query
    // ORDER BY), else a bare/qualified column key inherits its column's frozen collation; otherwise
    // the default `C` (byte) order. Resolved to the loaded `Collation` (42704 if not loaded). The
    // finalize sort applies it (an unmapped code point → 0A000 there).
    let collation: Option<std::sync::Arc<Collation>> = match &key.collation {
        Some(name) => {
            if !optype.collatable() {
                return Err(type_error(format!(
                    "collations are not supported by type {}",
                    optype.type_name()
                )));
            }
            resolve_collation_name(scope.catalog, name)?
        }
        // Otherwise the key's derived collation (collation.md §1): a column key's frozen collation,
        // an expression key's derivation; an indeterminate key is 42P21 (a sort target).
        None if optype.collatable() => {
            sort_target_collation(scope.catalog, order_key_deriv(scope, key)?)?
        }
        None => None,
    };

    let lname = name.to_string();
    let (plan, frac, result) = match lname.as_str() {
        "mode" => {
            // mode() takes no direct argument; mode(x) matches no overload (42883).
            if !args.is_empty() {
                return Err(no_agg_overload(&lname));
            }
            (AggPlan::OrderedSetMode, None, optype.clone())
        }
        "percentile_disc" => {
            // An ARRAY fraction (`percentile_disc(ARRAY[…])`) returns an array of percentiles, one
            // per element; a scalar fraction returns one value (aggregates.md §18).
            let (frac, is_array) = resolve_osa_fraction(scope, &lname, args, agg, params)?;
            let result = array_if(optype.clone(), is_array);
            (AggPlan::OrderedSetDisc, Some(frac), result)
        }
        "percentile_cont" => {
            // percentile_cont interpolates: over a NUMERIC input it widens to f64 and returns f64;
            // over an INTERVAL input it interpolates in the interval domain (PG `interval_lerp`) and
            // returns interval. Any other WITHIN GROUP type matches no overload (42883). An ARRAY
            // fraction makes the result an array of those percentiles (aggregates.md §18).
            let (frac, is_array) = resolve_osa_fraction(scope, &lname, args, agg, params)?;
            match optype {
                ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Float(_) => (
                    AggPlan::OrderedSetCont,
                    Some(frac),
                    array_if(ResolvedType::Float(ScalarType::Float64), is_array),
                ),
                ResolvedType::Interval => (
                    AggPlan::OrderedSetContInterval,
                    Some(frac),
                    array_if(ResolvedType::Interval, is_array),
                ),
                _ => return Err(no_agg_overload(&lname)),
            }
        }
        _ => unreachable!("is_ordered_set_aggregate_name gates the three names above"),
    };

    // FILTER (WHERE cond): resolved per input row with aggregates forbidden, exactly as for an
    // ordinary aggregate (aggregates.md §11) — a non-boolean cond is 42804, a nested aggregate
    // 42803. Composes with the ordered-set fold (the filter restricts the collected rows first).
    let rfilter = match filter {
        Some(f) => {
            let mut fsub = AggCtx::Forbidden;
            let (rf, ft) = resolve(scope, f, None, &mut fsub, params)?;
            match ft {
                ResolvedType::Bool | ResolvedType::Null => Some(rf),
                _ => {
                    return Err(EngineError::new(
                        SqlState::DatatypeMismatch,
                        "argument of FILTER must be type boolean",
                    )
                    .with_position(f.location().0));
                }
            }
        }
        None => None,
    };

    let osa = OsaParams {
        desc: key.descending,
        frac,
        collation,
    };
    match agg {
        AggCtx::Collect {
            group_keys, specs, ..
        } => {
            let slot = group_keys.len() + specs.len();
            specs.push(AggSpec {
                plan,
                operand: Some(operand),
                distinct: false,
                filter: rfilter,
                osa: Some(osa),
                hypo: None,
                order: None,
                collation: None,
                json_tys: Vec::new(),
            });
            Ok((RExpr::Column(slot), result))
        }
        AggCtx::GroupedWindow {
            group_keys,
            agg_specs,
            ..
        } => {
            let slot = group_keys.len() + agg_specs.len();
            agg_specs.push(AggSpec {
                plan,
                operand: Some(operand),
                distinct: false,
                filter: rfilter,
                osa: Some(osa),
                hypo: None,
                order: None,
                collation: None,
                json_tys: Vec::new(),
            });
            Ok((RExpr::Column(slot), result))
        }
        _ => unreachable!("the non-collecting context is rejected above"),
    }
}

/// Whether `name` is a hypothetical-set aggregate surface — `rank`/`dense_rank`/`percent_rank`/
/// `cume_dist` used with `WITHIN GROUP` (spec/design/aggregates.md §19). These names are *also*
/// window functions; the `WITHIN GROUP` clause routes them here instead of the window path.
pub(crate) fn is_hypothetical_set_name(name: &str) -> bool {
    matches!(
        name.to_string().as_str(),
        "rank" | "dense_rank" | "percent_rank" | "cume_dist"
    )
}

/// Resolve a hypothetical-set aggregate `f(direct_args) WITHIN GROUP (ORDER BY keys)` — `rank`,
/// `dense_rank`, `percent_rank`, `cume_dist` (spec/design/aggregates.md §19). The direct args are the
/// hypothetical row; the `WITHIN GROUP` keys are the sort columns. Their counts must match (else
/// `42883`). Each key operand is buffered per row; each direct arg is evaluated per group (it may
/// reference grouping columns) and coerced to the key's type. Like the other ordered-set aggregates,
/// `OVER` is `0A000`, `DISTINCT` is `42601`, and it is valid only in a collecting context.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_hypothetical_set_aggregate(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    keys: &[OrderKey],
    distinct: bool,
    filter: Option<&Expr>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if !matches!(agg, AggCtx::Collect { .. } | AggCtx::GroupedWindow { .. }) {
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate functions are not allowed here",
        ));
    }
    if distinct {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "DISTINCT is not allowed with ordered-set aggregates",
        ));
    }
    let lname = name.to_string();
    // The number of hypothetical direct arguments must match the number of ordering columns (PG
    // models a mismatch as a missing overload → 42883).
    if args.is_empty() || args.len() != keys.len() {
        return Err(no_agg_overload(&lname));
    }
    // Resolve each WITHIN GROUP key operand (per row, aggregates forbidden) + its sort spec, then the
    // matching direct argument (per group, in the grouped context so it may reference grouping
    // columns) coerced to the key's type.
    let mut key_nodes: Vec<RExpr> = Vec::with_capacity(keys.len());
    let mut sorts: Vec<KeySort> = Vec::with_capacity(keys.len());
    let mut arg_nodes: Vec<RExpr> = Vec::with_capacity(args.len());
    for (key, arg) in keys.iter().zip(args.iter()) {
        let (knode, ktype, sort) = resolve_agg_sort_key(scope, key, params)?;
        // The hypothetical direct arg, evaluated per group (grouped context); a literal adapts to the
        // key's scalar type via the hint. Its type must match the key's family (else 42883).
        let hint = match type_from_resolved(&ktype) {
            Ok(Type::Scalar(s)) => Some(s),
            _ => None,
        };
        let (anode, atype) = resolve(scope, arg, hint, agg, params)?;
        if !hypo_arg_compatible(&atype, &ktype) {
            return Err(no_agg_overload(&lname));
        }
        key_nodes.push(knode);
        sorts.push(sort);
        arg_nodes.push(anode);
    }

    // FILTER (WHERE cond): per-input-row predicate (aggregates forbidden); restricts buffered rows.
    let rfilter = resolve_agg_filter(scope, filter, params)?;

    let (plan, result) = match lname.as_str() {
        "rank" => (AggPlan::HypoRank, ResolvedType::Int(ScalarType::Int64)),
        "dense_rank" => (AggPlan::HypoDenseRank, ResolvedType::Int(ScalarType::Int64)),
        "percent_rank" => (
            AggPlan::HypoPercentRank,
            ResolvedType::Float(ScalarType::Float64),
        ),
        "cume_dist" => (
            AggPlan::HypoCumeDist,
            ResolvedType::Float(ScalarType::Float64),
        ),
        _ => unreachable!("is_hypothetical_set_name gates the four names above"),
    };
    let hypo = HypoParams {
        args: arg_nodes,
        keys: key_nodes,
        sorts,
    };
    match agg {
        AggCtx::Collect {
            group_keys, specs, ..
        } => {
            let slot = group_keys.len() + specs.len();
            specs.push(AggSpec {
                plan,
                operand: None,
                distinct: false,
                filter: rfilter,
                osa: None,
                hypo: Some(hypo),
                order: None,
                collation: None,
                json_tys: Vec::new(),
            });
            Ok((RExpr::Column(slot), result))
        }
        AggCtx::GroupedWindow {
            group_keys,
            agg_specs,
            ..
        } => {
            let slot = group_keys.len() + agg_specs.len();
            agg_specs.push(AggSpec {
                plan,
                operand: None,
                distinct: false,
                filter: rfilter,
                osa: None,
                hypo: Some(hypo),
                order: None,
                collation: None,
                json_tys: Vec::new(),
            });
            Ok((RExpr::Column(slot), result))
        }
        _ => unreachable!("the non-collecting context is rejected above"),
    }
}

/// Whether a hypothetical direct argument of type `arg` is comparable with the `WITHIN GROUP` key of
/// type `key` (aggregates.md §19). A `NULL` arg is always allowed; otherwise the two must be the same
/// scalar family (numeric `Int`/`Decimal`/`Float` interconvert, since the value comparator orders
/// them by value), so the buffered key tuple and the hypothetical row compare meaningfully.
pub(crate) fn hypo_arg_compatible(arg: &ResolvedType, key: &ResolvedType) -> bool {
    use ResolvedType::*;
    if matches!(arg, Null) {
        return true;
    }
    matches!(
        (arg, key),
        (Int(_), Int(_))
            | (Decimal, Decimal)
            | (Float(_), Float(_))
            | (Text, Text)
            | (Bool, Bool)
            | (Bytea, Bytea)
            | (Uuid, Uuid)
            | (Timestamp, Timestamp)
            | (Timestamptz, Timestamptz)
            | (Date, Date)
            | (Interval, Interval)
    )
}

/// Resolve an ordered-set aggregate's direct argument — the percentile fraction (aggregates.md
/// §13/§17). The fraction is evaluated **once per group**, so it may be any expression over
/// **grouping columns** (resolved here in the grouped `agg` context, so a grouping column binds its
/// synthetic key slot and a non-grouped column is `42803` — PG's *"direct arguments … must use only
/// grouped columns"*); a constant folds the usual way. An aggregate inside the fraction is `42803`
/// (PG forbids nesting). Resolved with a float hint so a bare numeric literal folds to `f64`. The
/// returned node is stored and evaluated per group at finalize. Returns `(node, is_array)` — a
/// NUMERIC array fraction (`percentile_cont(ARRAY[…])`) computes one percentile per element and
/// returns an array (§18). A non-numeric fraction or a wrong argument count matches no overload
/// (`42883`); a NULL fraction yields a NULL result at finalize.
pub(crate) fn resolve_osa_fraction(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, bool)> {
    let [arg] = args else {
        return Err(no_agg_overload(name)); // wrong argument count
    };
    // The fraction is evaluated before the fold (it is a direct argument, not an aggregate operand),
    // so a nested aggregate is illegal — 42803, matching PG.
    if expr_has_aggregate(arg) {
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate function calls cannot be nested",
        ));
    }
    let (rarg, rtype) = resolve(scope, arg, Some(ScalarType::Float64), agg, params)?;
    match rtype {
        ResolvedType::Null
        | ResolvedType::Float(_)
        | ResolvedType::Int(_)
        | ResolvedType::Decimal => Ok((rarg, false)),
        ResolvedType::Array(elem)
            if matches!(
                *elem,
                ResolvedType::Float(_) | ResolvedType::Int(_) | ResolvedType::Decimal
            ) =>
        {
            Ok((rarg, true))
        }
        _ => Err(no_agg_overload(name)), // a non-numeric fraction matches no overload
    }
}

/// `Array(t)` when `is_array`, else `t` — the result type of an ordered-set aggregate whose direct
/// argument is an array vs. a scalar fraction (aggregates.md §18).
pub(crate) fn array_if(t: ResolvedType, is_array: bool) -> ResolvedType {
    if is_array {
        ResolvedType::Array(Box::new(t))
    } else {
        t
    }
}

/// Resolve `GROUPING(c1, …, ck)` (spec/design/aggregates.md §12) — the grouping-sets membership
/// function. Valid only in a grouped query's projection / HAVING (`Collect`); each argument must be
/// one of the master grouping columns, else 42803 (matching PostgreSQL). Returns an `integer` (i32)
/// whose bit `(k-1-j)` is 1 iff `c_j` is grouped away in the row's grouping set. The value is computed
/// per group row at execution from the grouping set's mask, so the call resolves to the placeholder
/// slot `GROUPING_GS_BASE + index` (rebased to its real trailing synthetic slot after resolution).
pub(crate) fn resolve_grouping(
    scope: &Scope,
    args: &[Expr],
    star: bool,
    agg: &mut AggCtx,
    _params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if star {
        // GROUPING(*) — PG raises a syntax error; mirror the COUNT-only `*` message (42601).
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "* is only valid as the argument of COUNT",
        ));
    }
    if args.is_empty() {
        // GROUPING() with no arguments — PG raises a syntax error (42601).
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "GROUPING requires at least one argument",
        ));
    }
    let grouping_arg_err = || {
        EngineError::new(
            SqlState::GroupingError,
            "arguments to GROUPING must be grouping expressions of the associated query level",
        )
    };
    // GROUPING is meaningful only in a grouped query — `Collect`, or `GroupedWindow` when the query
    // also has window functions (GROUPING SETS + window, aggregates.md §21). Outside one (Forbidden /
    // Window) its arguments cannot be grouping expressions.
    let group_keys: Vec<usize> = match agg {
        AggCtx::Collect { group_keys, .. } | AggCtx::GroupedWindow { group_keys, .. } => {
            group_keys.clone()
        }
        _ => return Err(grouping_arg_err()),
    };
    // Map each argument (a grouping column) to its master-grouping-column position.
    let mut positions: Vec<usize> = Vec::with_capacity(args.len());
    for arg in args {
        let r = match arg {
            Expr::Column(name, pos) => scope.resolve_bare_at(name, *pos)?,
            Expr::QualifiedColumn {
                qualifier,
                name,
                pos,
            } => scope.resolve_qualified_at(qualifier, name, *pos)?,
            // A non-column argument is never a grouping column (jed groups by columns only).
            _ => return Err(grouping_arg_err()),
        };
        let idx = match r {
            Resolved::Local(idx) => idx,
            Resolved::Outer { .. } | Resolved::Layer { .. } => return Err(grouping_arg_err()),
        };
        match group_keys.iter().position(|&g| g == idx) {
            Some(p) => positions.push(p),
            None => return Err(grouping_arg_err()),
        }
    }
    let slot = match agg {
        AggCtx::Collect { grouping_specs, .. } | AggCtx::GroupedWindow { grouping_specs, .. } => {
            let s = GROUPING_GS_BASE + grouping_specs.len();
            grouping_specs.push(positions);
            s
        }
        _ => unreachable!("Collect / GroupedWindow verified above"),
    };
    Ok((RExpr::Column(slot), ResolvedType::Int(ScalarType::Int32)))
}

/// Resolve a column reference (already at real flat index `idx`) under an aggregate context.
/// In Forbidden mode it reads the real row directly; in Collect mode it must be a grouping key
/// — resolved to its synthetic-row slot (its position among the group keys) — else 42803.
pub(crate) fn collect_column(
    scope: &Scope,
    agg: &AggCtx,
    idx: usize,
    name: &str,
) -> Result<(RExpr, ResolvedType)> {
    let ty = resolved_type_of_col(&scope.column_at(idx).ty, scope.catalog);
    match agg {
        // Forbidden and Window both read the real input row by flat index (a plain window query's
        // bare columns are not grouped — spec/design/window.md §5.1).
        AggCtx::Forbidden | AggCtx::Window { .. } => Ok((RExpr::Column(idx), ty)),
        // Collect and GroupedWindow require a grouping key, resolved to its synthetic group-key slot.
        AggCtx::Collect { group_keys, .. } | AggCtx::GroupedWindow { group_keys, .. } => {
            match group_keys.iter().position(|&gk| gk == idx) {
                Some(pos) => Ok((RExpr::Column(pos), ty)),
                None => Err(grouping_error_column(name)),
            }
        }
    }
}

/// The single argument of a non-star aggregate call. Each aggregate takes exactly one
/// argument; a different count matches no aggregate overload and is 42883 (PG).
pub(crate) fn expect_arg(args: &[Expr]) -> Result<&Expr> {
    match args {
        [a] => Ok(a),
        _ => Err(EngineError::new(
            SqlState::UndefinedFunction,
            "no aggregate function matches the given argument count",
        )
        .hinted(error_hint::FUNCTION_NO_MATCH)),
    }
}

/// An aggregate over an operand family it has no overload for (e.g. SUM(text)) — 42883.
pub(crate) fn no_agg_overload(func: &str) -> EngineError {
    EngineError::new(
        SqlState::UndefinedFunction,
        format!("no {func} aggregate for that argument type"),
    )
    .hinted(error_hint::FUNCTION_NO_MATCH)
}

/// Whether `name` (exact, grammar.md §3) is a registered aggregate surface (COUNT/SUM/MIN/MAX/AVG).
/// Data-driven over the catalog (`AGGREGATES`); consulted by the grouping + CHECK-structure walks.
/// The ordered-set aggregates (`is_ordered_set_aggregate_name`) are aggregates for these purposes
/// too — they fold a set of rows — but are not catalog rows (their result/arg mold is special,
/// like `GROUPING()`), so they are OR'd in here rather than carried in `AGGREGATES`.
pub(crate) fn is_aggregate_name(name: &str) -> bool {
    AGGREGATES.iter().any(|a| surface_matches(a.surface, name))
        || object_agg_classify(name).is_some()
        || is_ordered_set_aggregate_name(name)
}

/// Classify a `json[b]_object_agg[_unique]` name → (is-json, is-unique). `None` otherwise. These
/// 2-argument aggregates are hand-resolved (the single-operand aggregate catalog can't express a
/// key/value pair), like `jsonb_set` among the scalar functions. (json-sql-functions.md §4)
pub(crate) fn object_agg_classify(name: &str) -> Option<(bool, bool)> {
    match name.to_string().as_str() {
        "jsonb_object_agg" => Some((false, false)),
        "json_object_agg" => Some((true, false)),
        "jsonb_object_agg_unique" => Some((false, true)),
        "json_object_agg_unique" => Some((true, true)),
        _ => None,
    }
}

/// Whether `name` is an ordered-set aggregate surface (`mode`/`percentile_cont`/`percentile_disc` —
/// spec/design/aggregates.md §13). These take a `WITHIN GROUP (ORDER BY …)` clause and are resolved
/// by `resolve_ordered_set_aggregate`, intercepted before the generic aggregate/scalar dispatch.
pub(crate) fn is_ordered_set_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_string().as_str(),
        "mode" | "percentile_cont" | "percentile_disc"
    )
}

/// Whether `name` is a registered WINDOW-only function surface (row_number/rank/…). Data-driven
/// over the catalog (`WINDOWS`). Such a function REQUIRES an `OVER` clause — used without one it is
/// 42P20 (spec/design/window.md §7). The catalog aggregates double as window functions but are not
/// in `WINDOWS`, so they are still valid without `OVER`.
pub(crate) fn is_window_only_name(name: &str) -> bool {
    WINDOWS.iter().any(|w| surface_matches(w.surface, name))
}

/// Resolve a window-function call `f(args) OVER (window_definition)` (spec/design/window.md §5.1).
/// Valid only in a window query's projection (`AggCtx::Window`); anywhere else (WHERE / JOIN ON /
/// HAVING / an aggregate query) it is 42P20. The call collects into a `WindowSpec` and resolves to
/// the synthetic slot `base + window_index`. S0: only `row_number()`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_window_call(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    star: bool,
    wd: &WindowDef,
    filter: Option<&Expr>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let lname = name.to_string();
    // Validate the context and build the sub-context window ARGUMENTS *and keys* resolve in
    // (spec/design/window.md §5.1). In a grouped query (`GroupedWindow`) they resolve against the
    // grouped row — a nested aggregate collects into the query's SHARED `agg_specs` and a bare column
    // must be a grouping key (else 42803) — so we resolve them under a `Collect` borrowing those
    // specs; a nested window is then 42P20 (a `Collect` cannot collect a window). In a plain window
    // query they resolve under `Forbidden` (no aggregate/window nesting). A non-column PARTITION BY /
    // ORDER BY key is materialized into the query-global `window_keys` (taken out here, restored at the
    // end). The window result's slot + the spec push happen at the end, once the def is resolved.
    let (mut sub, mut window_keys): (AggCtx, Vec<RExpr>) = match agg {
        AggCtx::GroupedWindow {
            group_keys,
            group_key_exprs,
            agg_specs,
            grouping_specs,
            window_keys,
            ..
        } => (
            AggCtx::Collect {
                group_keys: group_keys.clone(),
                group_key_exprs: group_key_exprs.clone(),
                specs: std::mem::take(agg_specs),
                grouping_specs: std::mem::take(grouping_specs),
            },
            std::mem::take(window_keys),
        ),
        AggCtx::Window { window_keys, .. } => (AggCtx::Forbidden, std::mem::take(window_keys)),
        _ => {
            return Err(EngineError::new(
                SqlState::WindowingError,
                "window functions are not allowed here",
            ));
        }
    };
    // The plan + result type from the function name. S0: only row_number(); an aggregate name with
    // OVER (a window aggregate) is deferred to S3; any other name is 42883.
    // The frame-insensitive no-argument ranking functions (S0/S1): row_number/rank/dense_rank → i64.
    let no_arg_i64 = match lname.as_str() {
        "row_number" => Some(WindowPlan::RowNumber),
        "rank" => Some(WindowPlan::Rank),
        "dense_rank" => Some(WindowPlan::DenseRank),
        _ => None,
    };
    // The frame-insensitive no-argument ratio functions (S1): percent_rank/cume_dist → f64
    // (PG's float8 — the ratio is the IEEE correctly-rounded f64 division, window.md §4).
    let no_arg_ratio = match lname.as_str() {
        "percent_rank" => Some(WindowPlan::PercentRank),
        "cume_dist" => Some(WindowPlan::CumeDist),
        _ => None,
    };
    let mut wargs: Vec<RExpr> = Vec::new();
    let mut agg_collation = None;
    let mut agg_json_tys = Vec::new();
    let (plan, result) = if let Some(p) = no_arg_i64 {
        if star || !args.is_empty() {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("{lname} takes no arguments"),
            )
            .hinted(error_hint::FUNCTION_NO_MATCH));
        }
        (p, ResolvedType::Int(ScalarType::Int64))
    } else if let Some(p) = no_arg_ratio {
        if star || !args.is_empty() {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("{lname} takes no arguments"),
            )
            .hinted(error_hint::FUNCTION_NO_MATCH));
        }
        (p, ResolvedType::Float(ScalarType::Float64))
    } else if lname == "ntile" {
        // ntile(n) — one integer bucket-count argument (window.md §4), resolved in a fresh
        // Forbidden sub-context (no aggregate/window nesting in a window argument).
        if star || args.len() != 1 {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                "ntile takes exactly one argument",
            )
            .hinted(error_hint::FUNCTION_NO_MATCH));
        }
        let (anode, aty) = resolve(scope, &args[0], None, &mut sub, params)?;
        if !matches!(aty, ResolvedType::Int(_) | ResolvedType::Null) {
            return Err(type_error("argument of ntile must be integer")
                .hinted(error_hint::FUNCTION_NO_MATCH));
        }
        wargs.push(anode);
        (WindowPlan::Ntile, ResolvedType::Int(ScalarType::Int64))
    } else if lname == "lag" || lname == "lead" {
        // lag/lead(value [, offset [, default]]) — window.md §4. The value expression's type is the
        // result; offset is an integer (default 1); default (returned when the offset leaves the
        // partition) must match the value type. Args resolved in a fresh Forbidden sub-context.
        if star || args.is_empty() || args.len() > 3 {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("{lname} takes 1 to 3 arguments"),
            )
            .hinted(error_hint::FUNCTION_NO_MATCH));
        }
        let (vnode, vty) = resolve(scope, &args[0], None, &mut sub, params)?;
        let hint = match &vty {
            ResolvedType::Int(s) | ResolvedType::Float(s) => Some(*s),
            _ => None,
        };
        wargs.push(vnode);
        if args.len() >= 2 {
            let (onode, oty) = resolve(scope, &args[1], None, &mut sub, params)?;
            if !matches!(oty, ResolvedType::Int(_) | ResolvedType::Null) {
                return Err(type_error(format!("offset of {lname} must be integer"))
                    .hinted(error_hint::FUNCTION_NO_MATCH));
            }
            wargs.push(onode);
        }
        if args.len() == 3 {
            let (dnode, dty) = resolve(scope, &args[2], hint, &mut sub, params)?;
            if dty != ResolvedType::Null && dty != vty {
                return Err(
                    type_error(format!("default of {lname} must match the value type"))
                        .hinted(error_hint::FUNCTION_NO_MATCH),
                );
            }
            wargs.push(dnode);
        }
        let plan = if lname == "lag" {
            WindowPlan::Lag
        } else {
            WindowPlan::Lead
        };
        (plan, vty)
    } else if is_aggregate_name(&lname) {
        // An aggregate used as a window function (S3): reuse the aggregate overload resolution to
        // get the plan + result type; apply_window_stage folds it over the default frame (running
        // with a window ORDER BY, whole-partition without — spec/design/window.md §6).
        let (aggplan, result) = if star {
            if !aggregate_has_star(&lname) {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "* is only valid as the argument of COUNT",
                ));
            }
            (AggPlan::CountStar, ResolvedType::Int(ScalarType::Int64))
        } else if args.len() >= 2 {
            // A multi-argument aggregate's arguments travel as one Row operand (aggregates.md §23).
            let (rargs, tys, desc) = resolve_agg_args(scope, &lname, args, &mut sub, params)?;
            let (plan, result) = aggregate_plan(&lname, desc.result, &tys[0]);
            wargs.push(RExpr::Row(rargs));
            (plan, result)
        } else {
            let (r, t) = resolve(scope, expect_arg(args)?, None, &mut sub, params)?;
            let desc =
                lookup_aggregate_overload(&lname, &t).ok_or_else(|| no_agg_overload(&lname))?;
            check_polymorphic_agg_operand(desc, &t)?;
            let (plan, result) = aggregate_plan(&lname, desc.result, &t);
            wargs.push(r); // the aggregate operand → args[0]
            if matches!(plan, AggPlan::JsonAgg { .. }) {
                agg_json_tys = vec![t];
            }
            (plan, result)
        };
        agg_collation = minmax_collation(scope, aggplan, args, &result)?;
        (WindowPlan::Agg(aggplan), result)
    } else if lname == "first_value" || lname == "last_value" || lname == "nth_value" {
        // Frame-sensitive value pickers (S4, window.md §4). first/last_value take one value
        // expression (→ result type); nth_value takes the value + an integer position.
        if star {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        let want = if lname == "nth_value" { 2 } else { 1 };
        if args.len() != want {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("{lname} takes {want} argument(s)"),
            )
            .hinted(error_hint::FUNCTION_NO_MATCH));
        }
        let (vnode, vty) = resolve(scope, &args[0], None, &mut sub, params)?;
        wargs.push(vnode);
        let plan = if lname == "first_value" {
            WindowPlan::FirstValue
        } else if lname == "last_value" {
            WindowPlan::LastValue
        } else {
            let (nnode, nty) = resolve(scope, &args[1], None, &mut sub, params)?;
            if !matches!(nty, ResolvedType::Int(_) | ResolvedType::Null) {
                return Err(type_error("position of nth_value must be integer")
                    .hinted(error_hint::FUNCTION_NO_MATCH));
            }
            wargs.push(nnode);
            WindowPlan::NthValue
        };
        (plan, vty)
    } else {
        // A known non-window function is PostgreSQL's 42809 (no hint); an unknown name is its
        // function-lookup failure (error-fields.md §11.7).
        let err = EngineError::new(
            SqlState::UndefinedFunction,
            format!("{lname} is not a window function"),
        );
        let known = is_scalar_func_name(&lname)
            || scope.catalog.session.env.functions().has_function(&lname);
        return Err(if known {
            err.no_hint()
        } else {
            err.hinted(error_hint::FUNCTION_NO_MATCH)
        });
    };
    // Resolve the window definition (PARTITION BY / ORDER BY expressions → slots, explicit frame).
    // Keys resolve in `sub` (the grouped Collect, so a bare grouping column → its grouped-row slot
    // and an aggregate → an agg slot, else 42803; or plain Forbidden, columns → real input slots); a
    // non-column key materializes into `window_keys` at a `WINDOW_KEY_BASE + k` placeholder. window.md
    // §5.1.
    let (partition, order, frame) =
        resolve_window_def(scope, wd, &mut sub, &mut window_keys, params)?;
    // FILTER (WHERE cond) on a window aggregate (aggregates.md §20): a per-frame-row boolean over the
    // INPUT row, resolved with aggregates forbidden (a nested aggregate is 42803, a non-boolean 42804)
    // — exactly the non-window FILTER rule (§11). The window stage folds only the frame rows it keeps.
    let rfilter = match filter {
        Some(f) => {
            let mut fsub = AggCtx::Forbidden;
            let (rf, ft) = resolve(scope, f, None, &mut fsub, params)?;
            match ft {
                ResolvedType::Bool | ResolvedType::Null => Some(rf),
                _ => {
                    return Err(type_error("argument of FILTER must be type boolean")
                        .with_position(f.location().0));
                }
            }
        }
        None => None,
    };
    let spec = WindowSpec {
        plan,
        partition,
        order,
        args: wargs,
        frame,
        filter: rfilter,
        agg_collation,
        agg_json_tys,
    };
    // Append the spec and resolve the result slot (the PLACEHOLDER `WINDOW_RESULT_BASE + w`, rebased to
    // its real slot after the row layout is final — window.md §5.1). Restore the borrowed `agg_specs`
    // (any aggregate nested in an argument or a window key was collected into `sub`) and the
    // materialized `window_keys`.
    match agg {
        AggCtx::GroupedWindow {
            agg_specs,
            grouping_specs,
            window_specs,
            window_keys: wk,
            ..
        } => {
            if let AggCtx::Collect {
                specs,
                grouping_specs: gs,
                ..
            } = sub
            {
                *agg_specs = specs;
                *grouping_specs = gs;
            }
            *wk = window_keys;
            let slot = WINDOW_RESULT_BASE + window_specs.len();
            window_specs.push(spec);
            Ok((RExpr::Column(slot), result))
        }
        AggCtx::Window {
            specs,
            window_keys: wk,
            ..
        } => {
            *wk = window_keys;
            let slot = WINDOW_RESULT_BASE + specs.len();
            specs.push(spec);
            Ok((RExpr::Column(slot), result))
        }
        // The entry match already rejected every other context with 42P20.
        _ => unreachable!("resolve_window_call entry validated the context"),
    }
}

/// Resolve the PARTITION BY and within-partition ORDER BY (→ sort keys) of an `OVER (...)` clause.
/// Each key is a **general expression** (spec/design/window.md §5.1) resolved against `key_ctx`: a
/// plain window query passes `Forbidden` (columns → real input slots, an aggregate is 42803), a
/// grouped one passes a `Collect` borrowing the query's aggregate specs (a bare column → its
/// grouping-column slot or 42803, an aggregate `sum(x)` collects → its agg slot). A bare-column /
/// aggregate key (`RExpr::Column`) keeps its real slot; any compound key is materialized into
/// `window_keys` at a `WINDOW_KEY_BASE + k` placeholder, evaluated per row before the window stage.
/// A key referencing an enclosing-query column (a correlated window) is rejected (`0A000`); a window
/// function inside a key is rejected by the `Forbidden`/`Collect` sub-context (`42P20`).
pub(crate) fn resolve_window_def(
    scope: &Scope,
    wd: &WindowDef,
    key_ctx: &mut AggCtx,
    window_keys: &mut Vec<RExpr>,
    params: &mut ParamTypes,
) -> Result<(
    Vec<usize>,
    Vec<crate::spill::SortKey>,
    Option<ResolvedFrame>,
)> {
    let mut partition = Vec::with_capacity(wd.partition.len());
    for key in &wd.partition {
        let (rexpr, ty) = resolve(scope, key, None, key_ctx, params)?;
        // Partitioning groups by byte equality, but an indeterminate key is still 42P21
        // (spec/design/collation.md §1 "A sort or grouping target").
        if ty.collatable() {
            reject_indeterminate_target(&derive_collation(scope, key)?)?;
        }
        partition.push(window_key_slot(rexpr, "PARTITION BY", window_keys)?);
    }
    let mut order: Vec<crate::spill::SortKey> = Vec::with_capacity(wd.order.len());
    // The ORDER BY key types, captured in lockstep with `order` — a `RANGE` value-offset frame folds
    // `key ± offset` over the single ordering key, so it needs the key's type (§6).
    let mut order_types: Vec<Type> = Vec::with_capacity(wd.order.len());
    for key in &wd.order {
        let (rexpr, ty) = resolve(scope, &key.expr, None, key_ctx, params)?;
        // The sort-key collation. An explicit trailing `COLLATE` (rare — `parse_expr` usually absorbs
        // a `COLLATE` into the key expression) must be on a text key (42804); otherwise the collation
        // is DERIVED from the key expression (collation.md §1) — a `COLLATE` inside it is explicit, a
        // bare text column is its frozen implicit collation, every other shape resets to none (C). A
        // collated window ORDER BY honors the collation in both the per-partition sort and peer
        // determination (window.md §3/§5); `COLLATE "C"` resolves to `None` (the raw-byte fast path).
        let coll = match &key.collation {
            Some(cn) => {
                if !ty.collatable() && ty != ResolvedType::Null {
                    return Err(type_error(format!(
                        "collations are not supported by type {}",
                        ty.type_name()
                    )));
                }
                resolve_collation_name(scope.catalog, cn)?
            }
            None => sort_target_collation(scope.catalog, derive_collation(scope, &key.expr)?)?,
        };
        let slot = window_key_slot(rexpr, "window ORDER BY", window_keys)?;
        order.push((slot, key.descending, key.nulls_first, coll));
        order_types.push(type_from_resolved(&ty)?);
    }
    // The explicit frame (window.md §6): ROWS / GROUPS integer-count offsets, RANGE value offsets.
    let frame = match &wd.frame {
        None => None,
        Some(f) => Some(resolve_frame(f, &order, &order_types)?),
    };
    Ok((partition, order, frame))
}

/// Validate every resolved `WINDOW`-clause entry's keys and frame (spec/design/window.md §5.3), even
/// an entry no `OVER` references — PostgreSQL's `transformWindowDefinitions` transforms the whole
/// clause, so an unreferenced entry's unknown column (`42703`), nested window function (`42P20`),
/// non-grouping column in a grouped query (`42803`), or bad frame is still an error. The keys resolve
/// exactly as a referenced window's do (`resolve_window_call`): under the query's grouped `Collect`
/// when it groups, else `Forbidden`. The resolution is discarded — any aggregate it collects is
/// truncated away and its materialized keys dropped — so validation changes no plan, result, or cost.
/// A negative frame offset (`22013`) is not an error here: PostgreSQL raises it only when an active
/// window runs, so an unreferenced entry carrying one is accepted.
pub(crate) fn validate_window_clause(
    scope: &Scope,
    windows: &[(String, WindowDef)],
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<()> {
    for (_, wd) in windows {
        let mut scratch_keys: Vec<RExpr> = Vec::new();
        match agg {
            AggCtx::Collect {
                group_keys,
                group_key_exprs,
                specs: agg_specs,
                grouping_specs,
            }
            | AggCtx::GroupedWindow {
                group_keys,
                group_key_exprs,
                agg_specs,
                grouping_specs,
                ..
            } => {
                let (n_aggs, n_groupings) = (agg_specs.len(), grouping_specs.len());
                let mut sub = AggCtx::Collect {
                    group_keys: group_keys.clone(),
                    group_key_exprs: group_key_exprs.clone(),
                    specs: std::mem::take(agg_specs),
                    grouping_specs: std::mem::take(grouping_specs),
                };
                let r =
                    resolve_window_def(scope, wd, &mut sub, &mut scratch_keys, params).map(|_| ());
                if let AggCtx::Collect {
                    mut specs,
                    grouping_specs: mut gs,
                    ..
                } = sub
                {
                    specs.truncate(n_aggs);
                    gs.truncate(n_groupings);
                    *agg_specs = specs;
                    *grouping_specs = gs;
                }
                accept_runtime_frame_error(r)?;
            }
            AggCtx::Window { .. } | AggCtx::Forbidden => {
                let mut sub = AggCtx::Forbidden;
                accept_runtime_frame_error(
                    resolve_window_def(scope, wd, &mut sub, &mut scratch_keys, params).map(|_| ()),
                )?;
            }
        }
    }
    Ok(())
}

/// Drop the one frame error PostgreSQL defers to execution (a negative offset, `22013`) from a
/// WINDOW-clause validation result (`validate_window_clause`).
fn accept_runtime_frame_error(r: Result<()>) -> Result<()> {
    match r {
        Err(e) if e.code() == "22013" => Ok(()),
        other => other,
    }
}

/// Map a resolved window-key expression to the slot the window stage indexes (spec/design/window.md
/// §5.1). A bare column / aggregate (`RExpr::Column`) keeps its real row slot — the input slot for a
/// plain query, the grouped-row slot for a grouped one — so a column-only window is byte-identical to
/// before. Any compound expression is materialized into `window_keys` at the placeholder slot
/// `WINDOW_KEY_BASE + k` (rebased once the row layout is final). A key referencing an enclosing query
/// (a correlated window — `where` names the clause) is the deferred follow-on (`0A000`).
pub(crate) fn window_key_slot(
    rexpr: RExpr,
    clause: &str,
    window_keys: &mut Vec<RExpr>,
) -> Result<usize> {
    if rexpr_references_outer(&rexpr, 0) {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!("{clause} may not reference an outer query column"),
        ));
    }
    Ok(match rexpr {
        RExpr::Column(i) => i,
        other => {
            let k = window_keys.len();
            window_keys.push(other);
            WINDOW_KEY_BASE + k
        }
    })
}

/// Resolve an explicit frame clause (spec/design/window.md §6). `GROUPS` requires an ORDER BY
/// (`42P20`); a `RANGE` value offset requires exactly one ORDER BY column (`42P20`) of an integer,
/// decimal, or float type (a timestamp/date key is the deferred D4 follow-on, any other type is
/// `0A000`). A negative offset is `22013`; `EXCLUDE` was already rejected at parse.
pub(crate) fn resolve_frame(
    f: &crate::ast::WindowFrame,
    order: &[crate::spill::SortKey],
    order_types: &[Type],
) -> Result<ResolvedFrame> {
    use crate::ast::{FrameBound, FrameMode};
    let is_offset =
        |b: &FrameBound| matches!(b, FrameBound::Preceding(_) | FrameBound::Following(_));
    let has_offset = is_offset(&f.start) || is_offset(&f.end);
    match f.mode {
        FrameMode::Rows => Ok(ResolvedFrame {
            mode: FrameMode::Rows,
            start: resolve_int_bound(&f.start)?,
            end: resolve_int_bound(&f.end)?,
            exclude: f.exclude,
        }),
        FrameMode::Groups => {
            if order.is_empty() {
                return Err(EngineError::new(
                    SqlState::WindowingError,
                    "GROUPS mode requires an ORDER BY clause",
                ));
            }
            Ok(ResolvedFrame {
                mode: FrameMode::Groups,
                start: resolve_int_bound(&f.start)?,
                end: resolve_int_bound(&f.end)?,
                exclude: f.exclude,
            })
        }
        FrameMode::Range if has_offset => {
            if order.len() != 1 {
                return Err(EngineError::new(
                    SqlState::WindowingError,
                    "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column",
                ));
            }
            let kt = &order_types[0];
            if !(kt.is_integer() || kt.is_decimal() || kt.is_float()) {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {}",
                        kt.canonical_name()
                    ),
                ));
            }
            Ok(ResolvedFrame {
                mode: FrameMode::Range,
                start: resolve_range_bound(&f.start, kt)?,
                end: resolve_range_bound(&f.end, kt)?,
                exclude: f.exclude,
            })
        }
        // RANGE with only UNBOUNDED / CURRENT ROW bounds — peer/edge based, any number of ORDER BY
        // keys (or none); no key arithmetic, so it reuses the plain bound resolution.
        FrameMode::Range => Ok(ResolvedFrame {
            mode: FrameMode::Range,
            start: resolve_int_bound(&f.start)?,
            end: resolve_int_bound(&f.end)?,
            exclude: f.exclude,
        }),
    }
}

/// Resolve a ROWS/GROUPS frame bound: the offset of `n PRECEDING`/`n FOLLOWING` must be a
/// non-negative integer literal (`22013` if negative; a non-literal/non-integer offset is `0A000`).
pub(crate) fn resolve_int_bound(b: &crate::ast::FrameBound) -> Result<ResolvedBound> {
    use crate::ast::FrameBound;
    let offset = |e: &Expr| -> Result<Value> {
        match e {
            Expr::Literal(Literal::Int(n), _) if *n >= 0 => Ok(Value::Int(*n)),
            Expr::Literal(Literal::Int(_), _) => Err(EngineError::new(
                SqlState::InvalidPrecedingOrFollowingSize,
                "frame starting or ending offset must not be negative",
            )),
            _ => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "frame offset must be a non-negative integer literal",
            )),
        }
    };
    Ok(match b {
        FrameBound::UnboundedPreceding => ResolvedBound::UnboundedPreceding,
        FrameBound::CurrentRow => ResolvedBound::CurrentRow,
        FrameBound::UnboundedFollowing => ResolvedBound::UnboundedFollowing,
        FrameBound::Preceding(e) => ResolvedBound::Preceding(offset(e)?),
        FrameBound::Following(e) => ResolvedBound::Following(offset(e)?),
    })
}

/// Resolve a RANGE value-offset bound (window.md §6). The offset literal must be a non-negative
/// numeric matching the ordering key type: an integer key takes an integer offset (a decimal offset
/// is `0A000`, matching PG); a decimal key takes an integer (widened) or decimal offset; a **float**
/// key takes an integer or decimal offset converted to `f64` (PG's `in_range_float*_float8` — the
/// offset is `float8` for both `f32` and `f64` keys, window.md §6). The decimal→`f64` conversion
/// traps `22003` on overflow (jed's float-cast rule, a negligible micro-divergence from PG's
/// accept-infinite-offset); an int offset is always finite.
pub(crate) fn resolve_range_bound(b: &crate::ast::FrameBound, kt: &Type) -> Result<ResolvedBound> {
    use crate::ast::FrameBound;
    let neg = || {
        EngineError::new(
            SqlState::InvalidPrecedingOrFollowingSize,
            "frame starting or ending offset must not be negative",
        )
    };
    let offset = |e: &Expr| -> Result<Value> {
        match e {
            Expr::Literal(Literal::Int(n), _) => {
                if *n < 0 {
                    return Err(neg());
                }
                if kt.is_float() {
                    Ok(Value::Float64(*n as f64))
                } else if kt.is_decimal() {
                    Ok(Value::Decimal(Decimal::from_i64(*n)))
                } else {
                    Ok(Value::Int(*n))
                }
            }
            Expr::Literal(Literal::Decimal(d), _) => {
                if d.is_negative() {
                    return Err(neg());
                }
                if kt.is_float() {
                    decimal_to_float(d, ScalarType::Float64)
                } else if kt.is_decimal() {
                    Ok(Value::Decimal(d.clone()))
                } else {
                    Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {} and offset type decimal",
                            kt.canonical_name()
                        ),
                    ))
                }
            }
            _ => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "frame offset must be a non-negative numeric literal",
            )),
        }
    };
    Ok(match b {
        FrameBound::UnboundedPreceding => ResolvedBound::UnboundedPreceding,
        FrameBound::CurrentRow => ResolvedBound::CurrentRow,
        FrameBound::UnboundedFollowing => ResolvedBound::UnboundedFollowing,
        FrameBound::Preceding(e) => ResolvedBound::Preceding(offset(e)?),
        FrameBound::Following(e) => ResolvedBound::Following(offset(e)?),
    })
}

/// A scalar function over argument types it has no overload for (e.g. abs(text), round(int,
/// text)) — 42883, like an aggregate with no matching overload.
pub(crate) fn no_func_overload(func: &str) -> EngineError {
    EngineError::new(
        SqlState::UndefinedFunction,
        format!("no {func} function for those argument types"),
    )
    .hinted(error_hint::FUNCTION_NO_MATCH)
}

/// The 42725 raised when an unknown-typed argument leaves several overloads of `func` standing
/// (functions.md §9.1) — the caller writes a cast to choose one.
pub(crate) fn ambiguous_func_overload(func: &str) -> EngineError {
    EngineError::new(
        SqlState::AmbiguousFunction,
        format!("function {func} is not unique for those argument types"),
    )
    .hinted(error_hint::FUNCTION_AMBIGUOUS)
}
