//! AST analysis that runs before resolution: whether a select list, ORDER BY or expression holds
//! an aggregate or a window call, and the desugaring of named windows (`WINDOW w AS (...)`,
//! `OVER w`) into inline definitions (window.md §3).

use super::*;

/// Whether any select item contains an aggregate call — i.e. this is an aggregate query.
pub(crate) fn items_have_aggregate(items: &SelectItems) -> bool {
    match items {
        SelectItems::All(_) => false,
        SelectItems::Items(items) => items.iter().any(|it| expr_has_aggregate(&it.expr)),
    }
}

/// Whether a window definition's PARTITION BY / ORDER BY keys contain an aggregate (`OVER (ORDER BY
/// sum(x))` — spec/design/window.md §5.1). Such an aggregate makes the query an aggregate query (a
/// whole-table aggregate if there is no GROUP BY), exactly as a top-level aggregate would, so the
/// window keys resolve against the grouped row. Used by both the inline-`over` walk in
/// `expr_has_aggregate` and the WINDOW-clause scan that computes `is_agg`.
pub(crate) fn window_def_has_aggregate(wd: &WindowDef) -> bool {
    wd.partition.iter().any(expr_has_aggregate)
        || wd.order.iter().any(|k| expr_has_aggregate(&k.expr))
}

/// Whether any WINDOW-clause entry's keys contain an aggregate (`WINDOW w AS (ORDER BY sum(x))`),
/// which — like a top-level aggregate — makes the query an aggregate query (spec/design/window.md
/// §5.1). The entries are still named references at this point (the OVER-name desugar runs later), so
/// the WINDOW clause is scanned directly.
pub(crate) fn windows_have_aggregate(windows: &[(String, WindowDef)]) -> bool {
    windows.iter().any(|(_, wd)| window_def_has_aggregate(wd))
}

/// The sub-expressions of one AST subscript spec (an index, or a slice's present bounds) — for the
/// `Expr` tree walkers.
pub(crate) fn subscript_spec_exprs(s: &SubscriptSpec) -> Vec<&Expr> {
    match s {
        SubscriptSpec::Index(i) => vec![i],
        SubscriptSpec::Slice(lo, hi) => lo.iter().chain(hi.iter()).collect(),
    }
}

/// Whether an expression tree contains an AGGREGATE call anywhere. A scalar-function call is
/// not itself an aggregate, but may CONTAIN one (`abs(sum(x))`), so its arguments are walked.
pub(crate) fn expr_has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::FuncCall {
            name,
            args,
            over,
            over_name,
            within_group,
            ..
        } => {
            // An aggregate name carrying OVER (inline or a named-window reference) is a WINDOW
            // function, not a bare aggregate (S3/S5, spec/design/window.md §5.1) — so it does not
            // make the query an aggregate query. (Detection runs before the OVER-name desugar.) But an
            // aggregate INSIDE its inline window definition's keys (`rank() OVER (ORDER BY sum(x))`)
            // does — those keys resolve against the grouped row (§5.1). A hypothetical-set name with a
            // WITHIN GROUP clause (`rank(x) WITHIN GROUP (…)`) is an aggregate (aggregates.md §19).
            (over.is_none() && over_name.is_none() && is_aggregate_name(name))
                || (within_group.is_some() && is_hypothetical_set_name(name))
                || args.iter().any(expr_has_aggregate)
                || over.as_deref().is_some_and(window_def_has_aggregate)
        }
        Expr::Column(_, _)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_, _)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_, _) => false,
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => expr_has_aggregate(inner),
        Expr::Collate { inner, .. } => expr_has_aggregate(inner),
        Expr::Unary { operand, .. } => expr_has_aggregate(operand),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_has_aggregate(operand),
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => {
            expr_has_aggregate(ctx)
                || expr_has_aggregate(path)
                || passing.iter().any(|p| expr_has_aggregate(&p.expr))
        }
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(rhs)
        }
        Expr::In { lhs, list, .. } => {
            expr_has_aggregate(lhs) || list.iter().any(expr_has_aggregate)
        }
        Expr::Quantified { lhs, array, .. } => expr_has_aggregate(lhs) || expr_has_aggregate(array),
        Expr::Between { lhs, lo, hi, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(lo) || expr_has_aggregate(hi)
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(rhs)
        }
        Expr::Row(items, _) | Expr::Array(items, _) => items.iter().any(expr_has_aggregate),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_has_aggregate(base),
        Expr::QualifiedStar { .. } => false,
        Expr::Subscript { base, subscripts } => {
            expr_has_aggregate(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(expr_has_aggregate)
        }
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => {
            operand.as_deref().is_some_and(expr_has_aggregate)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_aggregate(c) || expr_has_aggregate(r))
                || els.as_deref().is_some_and(expr_has_aggregate)
        }
        Expr::Coalesce(args, _) => args.iter().any(expr_has_aggregate),
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
            args.iter().any(expr_has_aggregate)
        }
        // A subquery is an independent query: an aggregate INSIDE it does not make the OUTER query
        // an aggregate query (the outer reference, if any, is just a constant to the subquery).
        Expr::ScalarSubquery(_, _)
        | Expr::Exists(_, _)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => false,
    }
}

/// Whether any select item contains a window-function call (a `FuncCall` carrying `OVER`). A
/// window query resolves its projection in `AggCtx::Window` mode (spec/design/window.md §5.1).
pub(crate) fn items_have_window(items: &SelectItems) -> bool {
    match items {
        SelectItems::All(_) => false,
        SelectItems::Items(items) => items.iter().any(|it| expr_has_window(&it.expr)),
    }
}

/// Whether any ORDER BY key is (or contains) a window function, so a query whose only `OVER` call
/// sits in the ORDER BY still sets up the window machinery (grammar.md §10, window.md §5.1). An
/// ordinal/column key carries no expression.
pub(crate) fn order_by_has_window(keys: &[OrderKey]) -> bool {
    keys.iter()
        .any(|k| k.expr.as_ref().is_some_and(expr_has_window))
}

/// Whether an expression tree contains a window-function call anywhere (a `FuncCall` whose `over`
/// is set). An ordinary call may CONTAIN one in its arguments (`abs(row_number() OVER ())`), so the
/// arguments are walked; a window call's own PARTITION BY / ORDER BY may not contain a window
/// function (that is rejected at resolve, 42P20), so they are not walked here.
pub(crate) fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::FuncCall {
            over,
            over_name,
            args,
            ..
        } => over.is_some() || over_name.is_some() || args.iter().any(expr_has_window),
        Expr::Column(_, _)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_, _)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_, _) => false,
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => expr_has_window(inner),
        Expr::Collate { inner, .. } => expr_has_window(inner),
        Expr::Unary { operand, .. } => expr_has_window(operand),
        Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => expr_has_window(operand),
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => {
            expr_has_window(ctx)
                || expr_has_window(path)
                || passing.iter().any(|p| expr_has_window(&p.expr))
        }
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            expr_has_window(lhs) || expr_has_window(rhs)
        }
        Expr::In { lhs, list, .. } => expr_has_window(lhs) || list.iter().any(expr_has_window),
        Expr::Quantified { lhs, array, .. } => expr_has_window(lhs) || expr_has_window(array),
        Expr::Between { lhs, lo, hi, .. } => {
            expr_has_window(lhs) || expr_has_window(lo) || expr_has_window(hi)
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            expr_has_window(lhs) || expr_has_window(rhs)
        }
        Expr::Row(items, _) | Expr::Array(items, _) => items.iter().any(expr_has_window),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => expr_has_window(base),
        Expr::QualifiedStar { .. } => false,
        Expr::Subscript { base, subscripts } => {
            expr_has_window(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(expr_has_window)
        }
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => {
            operand.as_deref().is_some_and(expr_has_window)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_window(c) || expr_has_window(r))
                || els.as_deref().is_some_and(expr_has_window)
        }
        Expr::Coalesce(args, _) => args.iter().any(expr_has_window),
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
            args.iter().any(expr_has_window)
        }
        // A subquery is an independent query: a window function inside it is the subquery's own.
        Expr::ScalarSubquery(_, _)
        | Expr::Exists(_, _)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => false,
    }
}

/// Desugar `OVER name` references in a select list to their WINDOW-clause definitions before
/// resolution (spec/design/window.md §5): each window call carrying `over_name` gets the named
/// definition copied into `over`; an undefined name is 42704. After this every window call carries
/// an inline `over`, so resolution (S0–S4) handles named and inline windows uniformly.
pub(crate) fn desugar_items(
    items: &mut SelectItems,
    windows: &[(String, WindowDef)],
) -> Result<()> {
    if let SelectItems::Items(v) = items {
        for it in v.iter_mut() {
            desugar_named_windows(&mut it.expr, windows)?;
        }
    }
    Ok(())
}

/// Apply the base-window merge rules (spec/design/window.md §5, PostgreSQL
/// `transformWindowDefinitions`): a definition that names a base copies the base's `PARTITION BY`
/// and — if the base has one — its `ORDER BY`, and supplies its own frame. The extender may **not**
/// add a `PARTITION BY` (42P20, even when the base has none), may add an `ORDER BY` only when the
/// base has none (42P20 otherwise), and the base must **not** carry a frame (42P20). The three
/// checks fire in PostgreSQL's priority order: PARTITION, then ORDER, then frame. Returns the
/// merged inline definition (`base = None`).
pub(crate) fn extend_window(
    base: &WindowDef,
    ext: &WindowDef,
    base_name: &str,
) -> Result<WindowDef> {
    if !ext.partition.is_empty() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot override PARTITION BY clause of window \"{base_name}\""),
        ));
    }
    if !base.order.is_empty() && !ext.order.is_empty() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot override ORDER BY clause of window \"{base_name}\""),
        ));
    }
    if base.frame.is_some() {
        return Err(EngineError::new(
            SqlState::WindowingError,
            format!("cannot copy window \"{base_name}\" because it has a frame clause"),
        ));
    }
    Ok(WindowDef {
        base: None,
        partition: base.partition.clone(),
        order: if base.order.is_empty() {
            ext.order.clone()
        } else {
            base.order.clone()
        },
        frame: ext.frame.clone(),
    })
}

/// Resolve a WINDOW clause into all-inline definitions (spec/design/window.md §5). Entries are
/// processed left-to-right; an entry naming a base extends an **already-resolved earlier** entry
/// (a self- or forward-reference is therefore "does not exist" — 42704), via `extend_window`. Every
/// entry is resolved — even ones no `OVER` references — matching PostgreSQL's whole-clause check.
pub(crate) fn resolve_window_clause(
    windows: &[(String, WindowDef)],
) -> Result<Vec<(String, WindowDef)>> {
    let mut resolved: Vec<(String, WindowDef)> = Vec::with_capacity(windows.len());
    for (name, def) in windows {
        // A name defined twice in one clause is 42P20 — checked before the base lookup, as
        // PostgreSQL's `transformWindowDefinitions` does.
        if resolved.iter().any(|(n, _)| n == name) {
            return Err(EngineError::new(
                SqlState::WindowingError,
                format!("window \"{name}\" is already defined"),
            ));
        }
        let r = if let Some(base_name) = &def.base {
            let base = lookup_window(&resolved, base_name)?;
            extend_window(&base, def, base_name)?
        } else {
            def.clone()
        };
        resolved.push((name.clone(), r));
    }
    Ok(resolved)
}

/// Find a (resolved, `base = None`) window definition by name in `windows`, byte-exactly, or
/// raise 42704 `window "<name>" does not exist`. Returns an owned clone to avoid borrow conflicts.
pub(crate) fn lookup_window(windows: &[(String, WindowDef)], name: &str) -> Result<WindowDef> {
    windows
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, d)| d.clone())
        .ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedObject,
                format!("window \"{name}\" does not exist"),
            )
        })
}

pub(crate) fn desugar_named_windows(e: &mut Expr, windows: &[(String, WindowDef)]) -> Result<()> {
    match e {
        Expr::FuncCall {
            over,
            over_name,
            args,
            ..
        } => {
            if let Some(name) = over_name.take() {
                // `OVER name` — a pure reference: copy the named definition whole, frame included
                // (no merge rules; copying a framed window is only forbidden for the parenthesized
                // extend form below — window.md §5).
                let def = lookup_window(windows, &name)?;
                *over = Some(Box::new(def));
            } else if over.as_ref().is_some_and(|d| d.base.is_some()) {
                // `OVER (base …)` — an extend: merge the inline definition onto the named base.
                let d = over.as_deref_mut().expect("base implies over is Some");
                let base_name = d.base.take().expect("base.is_some() checked");
                let base = lookup_window(windows, &base_name)?;
                *d = extend_window(&base, d, &base_name)?;
            }
            for a in args.iter_mut() {
                desugar_named_windows(a, windows)?;
            }
        }
        Expr::Cast { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Collate { inner, .. } => {
            desugar_named_windows(inner, windows)?;
        }
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => {
            desugar_named_windows(operand, windows)?;
        }
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => {
            desugar_named_windows(ctx, windows)?;
            desugar_named_windows(path, windows)?;
            for p in passing {
                desugar_named_windows(&mut p.expr, windows)?;
            }
        }
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(rhs, windows)?;
        }
        Expr::In { lhs, list, .. } => {
            desugar_named_windows(lhs, windows)?;
            for x in list.iter_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        Expr::Quantified { lhs, array, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(array, windows)?;
        }
        Expr::Between { lhs, lo, hi, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(lo, windows)?;
            desugar_named_windows(hi, windows)?;
        }
        Expr::Like { lhs, rhs, .. } | Expr::Regex { lhs, rhs, .. } => {
            desugar_named_windows(lhs, windows)?;
            desugar_named_windows(rhs, windows)?;
        }
        Expr::Row(items, _) | Expr::Array(items, _) => {
            for x in items.iter_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => {
            desugar_named_windows(base, windows)?;
        }
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => {
            if let Some(o) = operand.as_deref_mut() {
                desugar_named_windows(o, windows)?;
            }
            for (c, r) in whens.iter_mut() {
                desugar_named_windows(c, windows)?;
                desugar_named_windows(r, windows)?;
            }
            if let Some(x) = els.as_deref_mut() {
                desugar_named_windows(x, windows)?;
            }
        }
        Expr::Coalesce(args, _) => {
            for a in args.iter_mut() {
                desugar_named_windows(a, windows)?;
            }
        }
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
            for a in args.iter_mut() {
                desugar_named_windows(a, windows)?;
            }
        }
        // Leaves, subscripts, and subqueries (independent) carry no top-level window ref to rewrite.
        _ => {}
    }
    Ok(())
}
