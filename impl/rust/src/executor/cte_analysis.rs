//! `WITH` analysis: the shape and validity of a recursive CTE (recursive-cte.md), the walks
//! that count references to a CTE name (which decide inlining and self-reference rules), and
//! the synthetic table a CTE's output is resolved against.

use super::*;

// === WITH RECURSIVE analysis (spec/design/recursive-cte.md) ==========================
//
// A `WITH RECURSIVE` CTE is *recursive* iff its body references its own name (anywhere, deep). A
// recursive CTE must take the well-formed shape `non_recursive_term UNION [ALL] recursive_term`
// with the self-reference appearing exactly once, as a direct FROM/JOIN relation of the recursive
// term. These structural checks mirror PostgreSQL's `checkWellFormedRecursion`, run on the parsed
// AST before planning; the error surface is recursive-cte.md §6.

/// The recursiveness of a CTE in a `WITH RECURSIVE` list. `NonRecursive` = the body does not
/// reference the CTE (an ordinary CTE, even under `RECURSIVE`). `Recursive` = the validated
/// fixpoint shape, carrying the `UNION ALL` flag.
pub(crate) enum CteShape {
    NonRecursive,
    Recursive { union_all: bool },
}

/// Classify a CTE body for `WITH RECURSIVE` (recursive-cte.md §6). Returns `NonRecursive` when the
/// body does not reference `name`; otherwise validates the recursive shape and returns
/// `Recursive { union_all }`, or an error (`42P19` for a malformed recursion, `0A000` for a
/// deferred shape). With a `SEARCH` / `CYCLE` clause (`search_cycle`) each side of the `UNION` must
/// be a single SELECT (recursive-cte.md §9).
pub(crate) fn analyze_recursive_cte(
    name: &str,
    body: &QueryExpr,
    search_cycle: bool,
) -> Result<CteShape> {
    if count_self_refs_query(body, name) == 0 {
        return Ok(CteShape::NonRecursive);
    }
    // The body must be a top-level UNION / UNION ALL.
    let so = match body {
        QueryExpr::SetOp(so) if so.op == SetOpKind::Union => so,
        _ => {
            return Err(EngineError::new(
                SqlState::InvalidRecursion,
                format!(
                    "recursive query \"{name}\" does not have the form non-recursive-term UNION [ALL] recursive-term"
                ),
            ));
        }
    };
    // ORDER BY / LIMIT / OFFSET on a recursive query is not implemented (matching PostgreSQL).
    if !so.order_by.is_empty() {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "ORDER BY in a recursive query is not implemented".to_string(),
        ));
    }
    if so.limit.is_some() || so.offset.is_some() {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "LIMIT in a recursive query is not implemented".to_string(),
        ));
    }
    // The non-recursive (anchor) term — the UNION's left — must not reference the CTE.
    if count_self_refs_query(&so.lhs, name) > 0 {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            format!(
                "recursive reference to query \"{name}\" must not appear within its non-recursive term"
            ),
        ));
    }
    // SEARCH / CYCLE need each side of the UNION to be one SELECT (PostgreSQL's messages and codes).
    if search_cycle {
        if matches!(so.lhs, QueryExpr::SetOp(_)) {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "with a SEARCH or CYCLE clause, the left side of the UNION must be a SELECT",
            ));
        }
        if matches!(so.rhs, QueryExpr::SetOp(_)) {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "with a SEARCH or CYCLE clause, the right side of the UNION must be a SELECT",
            ));
        }
    }
    // The recursive term — the UNION's right — must be a plain SELECT (a set-operation recursive
    // term is a jed narrowing, 0A000) referencing the CTE exactly once, in a valid position.
    let rhs_sel = match &so.rhs {
        QueryExpr::Select(s) => s,
        QueryExpr::SetOp(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a set operation in the recursive term of a recursive query is not supported yet"
                    .to_string(),
            ));
        }
        QueryExpr::With(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a nested WITH in the recursive term of a recursive query is not supported yet"
                    .to_string(),
            ));
        }
    };
    validate_recursive_term(name, rhs_sel)?;
    Ok(CteShape::Recursive { union_all: so.all })
}

/// Validate the recursive term (the UNION's right SELECT) of a recursive CTE (recursive-cte.md §6).
/// The self-reference must appear exactly once, as a direct FROM/JOIN relation, not on the nullable
/// side of an outer join; the term must contain no aggregate. The checks fire in PostgreSQL's order
/// — a self-reference in a bad CONTEXT (a sublink, an outer join) is reported as that context even
/// when a valid FROM reference also exists, so context checks precede the once-only count.
pub(crate) fn validate_recursive_term(name: &str, sel: &Select) -> Result<()> {
    // A self-reference inside an expression sublink is `within a subquery` (matching PostgreSQL),
    // regardless of any valid FROM reference also present.
    if count_sublink_self_refs(sel, name) >= 1 {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            format!("recursive reference to query \"{name}\" must not appear within a subquery"),
        ));
    }
    // A self-reference nested inside a FROM derived table is a jed narrowing (0A000) — PostgreSQL
    // allows `FROM (… c …)`.
    if count_from_subquery_self_refs(sel, name) >= 1 {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!(
                "recursive reference to query \"{name}\" inside a FROM subquery is not supported yet"
            ),
        ));
    }
    // The remaining self-references are all direct FROM/JOIN relations.
    let direct = count_direct_from_self_refs(sel, name);
    if direct > 1 {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            format!("recursive reference to query \"{name}\" must not appear more than once"),
        ));
    }
    // An aggregate in the recursive term is rejected (matching PostgreSQL).
    if items_have_aggregate(&sel.items) || sel.having.as_ref().is_some_and(expr_has_aggregate) {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            "aggregate functions are not allowed in a recursive query's recursive term".to_string(),
        ));
    }
    // A direct reference on the nullable side of an outer join is `within an outer join`.
    if direct == 1 && direct_self_ref_on_nullable_side(sel, name) {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            format!("recursive reference to query \"{name}\" must not appear within an outer join"),
        ));
    }
    Ok(())
}

/// Self-references reachable only through an expression sublink (a scalar/`IN`/`EXISTS`/quantified
/// subquery) in this SELECT's top-level expressions — the `within a subquery` position.
pub(crate) fn count_sublink_self_refs(s: &Select, name: &str) -> usize {
    select_exprs(s)
        .iter()
        .map(|e| cte_refs_expr(e, &[], &mut |r| usize::from(r == name)))
        .sum()
}

/// Check a recursive CTE's column types (recursive-cte.md §2): the output types are FIXED by the
/// non-recursive (anchor) term, and the recursive term's columns must be assignable to them — a
/// literal adapts, an equal type passes, a WIDER type is `42804` (matching PostgreSQL). Mechanically
/// the would-be UNION unified type must EQUAL the anchor type; any widening of the anchor is the
/// error. An arity mismatch is `42601`, like a plain UNION. A `UNION` (not `UNION ALL`) recursive
/// CTE dedups its rows, so each column also needs an equality operator (42883 for json/jsonpath).
/// `carried` is the number of hidden trailing outputs a SEARCH / CYCLE recursive term carries
/// (recursive-cte.md §9), which are not compared.
pub(crate) fn check_recursive_column_types(
    anchor_ast: &QueryExpr,
    anchor: &QueryPlan,
    recursive: &QueryPlan,
    name: &str,
    union_all: bool,
    carried: usize,
) -> Result<()> {
    let a = anchor.column_types();
    let all = recursive.column_types();
    let r = &all[..all.len() - carried];
    if a.len() != r.len() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "each UNION query must have the same number of columns".to_string(),
        ));
    }
    for (i, (at, rt)) in a.iter().zip(r.iter()).enumerate() {
        // PostgreSQL reports a column the recursive term changes at the non-recursive term's column.
        let at_col = plan_query::output_column_location(anchor_ast, i).0;
        let unified = unify_setop_column(at, rt, SetOpKind::Union)?;
        if &unified != at {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                format!(
                    "recursive query \"{name}\" column {} has type {} in non-recursive term but type {} overall",
                    i + 1,
                    at.type_name(),
                    unified.type_name(),
                ),
            )
            .hinted(error_hint::RECURSIVE_TERM_TYPE)
            .with_position(at_col));
        }
        // The non-recursive term also fixed the column's typmod, so the overall common typmod must
        // equal it (PostgreSQL compares type and typmod — create-table-as.md §2).
        let at_mod = anchor.column_typmods()[i];
        let overall_mod = common_typmod([at_mod, recursive.column_typmods()[i]]);
        if overall_mod != at_mod {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                format!(
                    "recursive query \"{name}\" column {} has type {} in non-recursive term but type {} overall",
                    i + 1,
                    typmod_type_name(at, at_mod),
                    typmod_type_name(at, overall_mod),
                ),
            )
            .hinted(error_hint::RECURSIVE_TERM_TYPE)
            .with_position(at_col));
        }
        if !union_all {
            require_setop_equality(at)?;
        }
        // The non-recursive term fixed the relation's collation, so the UNION's overall common
        // collation must equal it (PostgreSQL's recursive-CTE check, spec/design/collation.md §1):
        // a recursive term cannot re-collate a column. Mismatch is 42P21.
        if at.collatable() {
            let ad = &anchor.column_derivs()[i];
            let overall = common_collation(
                &[ad.clone(), recursive.column_derivs()[i].clone()],
                union_all,
            )?;
            let (non_rec, all) = (collation_identity(ad), collation_identity(&overall));
            if non_rec != all {
                return Err(EngineError::new(
                    SqlState::CollationMismatch,
                    format!(
                        "recursive query \"{name}\" column {} has collation \"{non_rec}\" in non-recursive term but collation \"{all}\" overall",
                        i + 1,
                    ),
                )
                .hinted(error_hint::RECURSIVE_TERM_COLLATION)
                .with_position(at_col));
            }
        }
    }
    Ok(())
}

/// A resolved type's name with its typmod applied (`decimal(10,2)`, `varchar(5)`), as a column of it
/// would be declared.
pub(crate) fn typmod_type_name(ty: &ResolvedType, typmod: Typmod) -> String {
    match (ty, typmod.decimal, typmod.varchar_len) {
        (ResolvedType::Decimal, Some(d), _) => format!("decimal({},{})", d.precision, d.scale),
        (ResolvedType::Text, _, Some(n)) => format!("varchar({n})"),
        _ => ty.type_name(),
    }
}

/// The collation a derivation settles to, named as PostgreSQL reports it (the recursive-CTE check,
/// spec/design/collation.md §1): a collation's name, `default` for no collation (PG's default
/// collation — a literal's), `(null)` for a conflicted / indeterminate one (PG's InvalidOid).
pub(crate) fn collation_identity(d: &Deriv) -> &str {
    match d {
        Deriv::Implicit(n) | Deriv::Explicit(n) => n,
        Deriv::None => "default",
        Deriv::Indeterminate | Deriv::Conflicted => "(null)",
    }
}

/// Total self-references to `name` anywhere in a query expression (deep — FROM relations at every
/// nesting level plus expression sublinks).
pub(crate) fn cte_refs_query(
    qe: &QueryExpr,
    shadowed: &[&str],
    visit: &mut CteRefVisitor,
) -> usize {
    match qe {
        QueryExpr::Select(s) => cte_refs_select(s, shadowed, visit),
        QueryExpr::SetOp(so) => {
            cte_refs_query(&so.lhs, shadowed, visit) + cte_refs_query(&so.rhs, shadowed, visit)
        }
        QueryExpr::With(we) => {
            // A nested WITH inherits this binding until an inner same-named CTE is declared. That
            // inner binding shadows it for later bodies and the main query, but its own
            // non-recursive body still sees the inherited binding (cte.md §7).
            let mut n = 0;
            let mut inner: Vec<&str> = shadowed.to_vec();
            for cte in &we.ctes {
                n += cte_refs_dml(&cte.body, &inner, visit);
                inner.push(cte.name.as_str());
            }
            n += cte_refs_query(&we.body, &inner, visit);
            n
        }
    }
}

/// Total self-references in a SELECT: its FROM relations (deep) plus all of its expressions' sublinks.
pub(crate) fn cte_refs_select(s: &Select, shadowed: &[&str], visit: &mut CteRefVisitor) -> usize {
    let mut n = 0;
    for tref in from_relations(s) {
        n += cte_refs_tableref(tref, shadowed, visit);
    }
    for e in select_exprs(s) {
        n += cte_refs_expr(e, shadowed, visit);
    }
    n
}

/// Self-references reachable through one FROM relation: a plain table reference with the matching
/// name (+1), a derived-table subquery (recurse), or a table-function's / VALUES' argument exprs.
pub(crate) fn cte_refs_tableref(
    tref: &TableRef,
    shadowed: &[&str],
    visit: &mut CteRefVisitor,
) -> usize {
    if is_plain_relation(tref) {
        if shadowed.contains(&tref.name.as_str()) {
            return 0;
        }
        return visit(&tref.name);
    }
    let mut n = 0;
    if let Some(sub) = &tref.subquery {
        n += cte_refs_query(sub, shadowed, visit);
    }
    if let Some(args) = &tref.args {
        for a in args {
            n += cte_refs_expr(a, shadowed, visit);
        }
    }
    if let Some(rows) = &tref.values {
        for row in rows {
            for e in row {
                n += cte_refs_expr(e, shadowed, visit);
            }
        }
    }
    n
}

/// Self-references inside an expression — only reachable through a sublink (a subquery is an
/// independent query whose own FROM may reference the CTE). Walks every Expr variant to find the
/// sublinks, then recurses into each sublink's query. The walk is exhaustive (like
/// `expr_has_aggregate`) so a new Expr variant is a compile error here, not a silent miss.
pub(crate) fn cte_refs_expr(e: &Expr, shadowed: &[&str], visit: &mut CteRefVisitor) -> usize {
    let mut sub = |x: &Expr| cte_refs_expr(x, shadowed, visit);
    match e {
        Expr::ScalarSubquery(q, _) | Expr::Exists(q, _) => cte_refs_query(q, shadowed, visit),
        Expr::InSubquery { lhs, query, .. } | Expr::QuantifiedSubquery { lhs, query, .. } => {
            sub(lhs) + cte_refs_query(query, shadowed, visit)
        }
        Expr::Column(_, _)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_, _)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_, _) => 0,
        Expr::Row(items, _) | Expr::Array(items, _) => items.iter().map(&mut sub).sum(),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => sub(base),
        // `t.*` is a leaf (a relation name, no sub-expression) — no sublink to recurse into.
        Expr::QualifiedStar { .. } => 0,
        Expr::Subscript { base, subscripts } => {
            sub(base)
                + subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .map(&mut sub)
                    .sum::<usize>()
        }
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. } => sub(inner),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => sub(operand),
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => sub(ctx) + sub(path) + passing.iter().map(|p| sub(&p.expr)).sum::<usize>(),
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => sub(lhs) + sub(rhs),
        Expr::In { lhs, list, .. } => sub(lhs) + list.iter().map(&mut sub).sum::<usize>(),
        Expr::Quantified { lhs, array, .. } => sub(lhs) + sub(array),
        Expr::Between { lhs, lo, hi, .. } => sub(lhs) + sub(lo) + sub(hi),
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => {
            operand.as_deref().map_or(0, &mut sub)
                + whens.iter().map(|(c, r)| sub(c) + sub(r)).sum::<usize>()
                + els.as_deref().map_or(0, &mut sub)
        }
        Expr::Coalesce(args, _) => args.iter().map(&mut sub).sum(),
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
            args.iter().map(&mut sub).sum()
        }
        Expr::FuncCall { args, .. } => args.iter().map(&mut sub).sum(),
    }
}

/// Whether a `WITH` statement contains any data-modifying part — a data-modifying CTE body or a
/// data-modifying primary (spec/design/writable-cte.md). Such a statement runs through the
/// writable-CTE orchestrator (the read pin + lexical-order, all-or-nothing execution); a pure-query
/// `WITH` keeps the [`Engine::run_with`] path.
pub(crate) fn with_has_dml(wq: &WithQuery) -> bool {
    wq.body.is_data_modifying() || wq.ctes.iter().any(|c| c.body.is_data_modifying())
}

/// Each CTE binding's evaluation mode (spec/design/cte.md §3, writable-cte.md §3): a RECURSIVE or
/// data-modifying CTE is ALWAYS materialized; otherwise a `MATERIALIZED` hint or ≥2 references →
/// Materialize, else Inline.
pub(crate) fn cte_modes(bindings: &[CteBinding]) -> Vec<CteMode> {
    bindings
        .iter()
        .map(|b| {
            if b.recursive.is_some() || matches!(b.source, CteSource::Dml(_)) {
                return CteMode::Materialize;
            }
            // A body planned at its reference is inlined there (planner.md §3.4), even if the
            // static count missed a reference the planner then resolved.
            if b.expanded.get() {
                return CteMode::Inline;
            }
            match b.hint {
                Some(true) => CteMode::Materialize,
                Some(false) => CteMode::Inline,
                None if b.refs.get() >= 2 => CteMode::Materialize,
                None => CteMode::Inline,
            }
        })
        .collect()
}

/// Add `extra` cost to an outcome (the writable-CTE orchestrator folds the materialization cost of
/// the data-modifying / query CTEs into the primary's result — spec/design/writable-cte.md §8).
pub(crate) fn add_outcome_cost(outcome: Outcome, extra: i64) -> Outcome {
    match outcome {
        Outcome::Query {
            column_names,
            column_types,
            rows,
            cost,
        } => Outcome::Query {
            column_names,
            column_types,
            rows,
            cost: cost + extra,
        },
        Outcome::Statement {
            cost,
            rows_affected,
        } => Outcome::Statement {
            cost: cost + extra,
            rows_affected,
        },
    }
}

/// References to CTE `name` reachable through a `cte_body`'s inner queries — the writable-CTE
/// analogue of [`count_self_refs_query`] (spec/design/writable-cte.md §3). A query body delegates to
/// the query counter; a data-modifying body counts the references in its source query / `WHERE` /
/// `SET` RHSs / `ON CONFLICT` / `RETURNING` sublinks. Used by the orchestrator to count the
/// references a NON-planned data-modifying part contributes to the inline-vs-materialize decision.
pub(crate) fn cte_refs_dml(body: &CteBody, shadowed: &[&str], visit: &mut CteRefVisitor) -> usize {
    match body {
        CteBody::Query(q) => cte_refs_query(q, shadowed, visit),
        CteBody::Insert(ins) => {
            let mut n = match &ins.source {
                InsertSource::Query(q) => cte_refs_query(q, shadowed, visit),
                // A VALUES expression slot may hold a sublink (grammar.md §16).
                InsertSource::Values(rows) => rows
                    .iter()
                    .flatten()
                    .map(|v| match v {
                        InsertValue::Expr(e) => cte_refs_expr(e, shadowed, visit),
                        _ => 0,
                    })
                    .sum(),
            };
            if let Some(oc) = &ins.on_conflict
                && let ConflictAction::DoUpdate {
                    assignments,
                    filter,
                } = &oc.action
            {
                for a in assignments {
                    n += cte_refs_expr(&a.value, shadowed, visit);
                }
                if let Some(f) = filter {
                    n += cte_refs_expr(f, shadowed, visit);
                }
            }
            n + cte_refs_returning(&ins.returning, shadowed, visit)
        }
        CteBody::Update(upd) => {
            let mut n = 0;
            for a in &upd.assignments {
                n += cte_refs_expr(&a.value, shadowed, visit);
            }
            if let Some(f) = &upd.filter {
                n += cte_refs_expr(f, shadowed, visit);
            }
            n + cte_refs_returning(&upd.returning, shadowed, visit)
        }
        CteBody::Delete(del) => {
            let mut n = 0;
            if let Some(f) = &del.filter {
                n += cte_refs_expr(f, shadowed, visit);
            }
            n + cte_refs_returning(&del.returning, shadowed, visit)
        }
        CteBody::Merge(m) => {
            use crate::ast::{MergeAction, MergeValue};
            let mut n =
                cte_refs_query(&m.source, shadowed, visit) + cte_refs_expr(&m.on, shadowed, visit);
            for c in &m.clauses {
                if let Some(cond) = &c.condition {
                    n += cte_refs_expr(cond, shadowed, visit);
                }
                match &c.action {
                    MergeAction::Update(assigns) => {
                        for a in assigns {
                            n += cte_refs_expr(&a.value, shadowed, visit);
                        }
                    }
                    MergeAction::Insert { values, .. } => {
                        for v in values {
                            if let MergeValue::Expr(e) = v {
                                n += cte_refs_expr(e, shadowed, visit);
                            }
                        }
                    }
                    MergeAction::Delete | MergeAction::DoNothing => {}
                }
            }
            n + cte_refs_returning(&m.returning, shadowed, visit)
        }
    }
}

/// References to CTE `name` in a `RETURNING` item list's sublinks.
pub(crate) fn cte_refs_returning(
    returning: &Option<ReturningClause>,
    shadowed: &[&str],
    visit: &mut CteRefVisitor,
) -> usize {
    match returning {
        Some(ReturningClause {
            items: SelectItems::Items(items),
            ..
        }) => items
            .iter()
            .map(|it| cte_refs_expr(&it.expr, shadowed, visit))
            .sum(),
        _ => 0,
    }
}

/// Self-references that are DIRECT FROM/JOIN relations of this SELECT (a plain table ref matching
/// the name, not nested in a subquery). This is the only valid position for a recursive reference.
pub(crate) fn count_direct_from_self_refs(s: &Select, name: &str) -> usize {
    from_relations(s)
        .filter(|tref| is_plain_relation(tref) && tref.name == name)
        .count()
}

/// Self-references nested inside a FROM-position subquery / table-function args / VALUES of this
/// SELECT (the deferred `0A000` shape — PostgreSQL allows a self-reference in `FROM (… c …)`).
pub(crate) fn count_from_subquery_self_refs(s: &Select, name: &str) -> usize {
    from_relations(s)
        .filter(|tref| !is_plain_relation(tref))
        .map(|tref| cte_refs_tableref(tref, &[], &mut |r| usize::from(r == name)))
        .sum()
}

/// Whether the SELECT's single direct self-reference sits on the NULLABLE side of an outer join —
/// the position PostgreSQL rejects (`within an outer join`). The FROM is a left-deep chain: relation
/// 0 is `from`, relation `i+1` is `joins[i].table`, combined by `joins[i].kind`. A LEFT/FULL join
/// makes its right operand nullable; a RIGHT/FULL join makes the whole accumulated left nullable.
pub(crate) fn direct_self_ref_on_nullable_side(s: &Select, name: &str) -> bool {
    let rels: Vec<&TableRef> = from_relations(s).collect();
    let mut nullable = vec![false; rels.len()];
    for (j, jc) in s.joins.iter().enumerate() {
        let right = j + 1;
        match jc.kind {
            JoinKind::Left => nullable[right] = true,
            JoinKind::Right => nullable.iter_mut().take(right).for_each(|n| *n = true),
            JoinKind::Full => nullable.iter_mut().take(right + 1).for_each(|n| *n = true),
            JoinKind::Inner | JoinKind::Cross => {}
        }
    }
    rels.iter()
        .enumerate()
        .any(|(i, tref)| is_plain_relation(tref) && tref.name == name && nullable[i])
}

/// A FROM relation that is a plain table NAME — not a derived-table subquery, a table function, or
/// a VALUES body. Only a plain relation can resolve to a CTE.
pub(crate) fn is_plain_relation(tref: &TableRef) -> bool {
    tref.subquery.is_none() && tref.args.is_none() && tref.values.is_none()
}

/// The FROM relations of a SELECT in left-deep order: `from` (if present) then each join's table.
pub(crate) fn from_relations(s: &Select) -> impl Iterator<Item = &TableRef> {
    s.from.iter().chain(s.joins.iter().map(|j| &j.table))
}

/// Every top-level expression of a SELECT that can hold a sublink (select items, WHERE, GROUP BY,
/// HAVING, join ON conditions) — for the sublink self-reference walk. ORDER BY keys are bare /
/// qualified column references (never expressions — `OrderKey`), so they carry no sublink.
pub(crate) fn select_exprs(s: &Select) -> Vec<&Expr> {
    let mut v: Vec<&Expr> = Vec::new();
    if let SelectItems::Items(items) = &s.items {
        v.extend(items.iter().map(|it| &it.expr));
    }
    v.extend(s.filter.iter());
    for item in &s.group_by {
        item.for_each_expr(&mut |e| v.push(e));
    }
    v.extend(s.having.iter());
    v.extend(s.joins.iter().filter_map(|j| j.on.as_ref()));
    v
}

/// Build the synthetic relation a CTE reference resolves against (spec/design/cte.md §2): one
/// column per body output, named by the rename list (a count mismatch is 42P10) or the body's own
/// output names, typed from the planned body. The relation has no primary key / constraints — it is
/// read-only and its rows come from the CTE context, never a store. Each column carries its body
/// output's collation derivation (spec/design/collation.md §1 "Derived relations").
pub(crate) fn cte_synthetic_table(
    name: &str,
    plan: &QueryPlan,
    rename: Option<&[String]>,
) -> Result<Box<Table>> {
    cte_synthetic_table_cols(
        name,
        plan.column_names(),
        plan.column_types(),
        Some(plan.column_derivs()),
        Some(plan.column_typmods()),
        rename,
    )
}

/// The shared core of [`cte_synthetic_table`], over explicit body column names + types + collation
/// derivations + typmods (`None` = none for every column) — so a data-modifying CTE (whose "body
/// output" is its `RETURNING` projection, not a `QueryPlan`) builds its synthetic relation the same
/// way (spec/design/writable-cte.md §1). Each column records its body output's typmod, so a
/// reference to it keeps the typmod (spec/design/create-table-as.md §2).
pub(crate) fn cte_synthetic_table_cols(
    name: &str,
    body_names: &[String],
    body_types: &[ResolvedType],
    body_derivs: Option<&[Deriv]>,
    body_typmods: Option<&[Typmod]>,
    rename: Option<&[String]>,
) -> Result<Box<Table>> {
    let col_names: Vec<String> = match rename {
        // PostgreSQL allows FEWER aliases than the body has columns — the first `cols.len()` columns
        // take the aliases, the rest keep their body output names (a partial rename). Only MORE
        // aliases than columns is an error (42P10).
        Some(cols) => {
            if cols.len() > body_types.len() {
                return Err(EngineError::new(
                    SqlState::InvalidColumnReference,
                    format!(
                        "WITH query \"{name}\" has {} columns available but {} columns specified",
                        body_types.len(),
                        cols.len()
                    ),
                ));
            }
            (0..body_types.len())
                .map(|i| {
                    cols.get(i)
                        .cloned()
                        .unwrap_or_else(|| body_names[i].clone())
                })
                .collect()
        }
        None => body_names.to_vec(),
    };
    let columns = col_names
        .iter()
        .zip(body_types.iter())
        .enumerate()
        .map(|(i, (n, t))| {
            let mut col = Column {
                name: n.clone(),
                ty: type_from_resolved(t)?,
                decimal: None,
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
            };
            let d = body_derivs.map_or(&Deriv::None, |ds| &ds[i]);
            set_synthetic_collation(&mut col, d);
            let t = body_typmods.map_or(Typmod::NONE, |ts| ts[i]);
            col.decimal = t.decimal;
            col.varchar_len = t.varchar_len;
            col.temporal = t.temporal;
            Ok(col)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Box::new(Table {
        name: name.to_string(),
        columns,
        pk: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        exclusions: Vec::new(),
        on_commit: crate::catalog::OnCommit::PreserveRows,
        scalar_func: false,
    }))
}

/// Record a body output's collation derivation on its synthetic column (spec/design/collation.md §1
/// "Derived relations", PostgreSQL's rule): an implicit OR explicit body collation becomes the
/// column's IMPLICIT collation (an outer reference is a plain column reference); any other body
/// output has no collation and takes a neighbour's (PG's default). An indeterminate or conflicted
/// body output has none either (PG's InvalidOid) — it still yields to a neighbour, but consuming it
/// alone is 42P22.
pub(crate) fn set_synthetic_collation(col: &mut Column, d: &Deriv) {
    match d {
        Deriv::Implicit(name) | Deriv::Explicit(name) => {
            if name != "C" {
                col.collation = Some(name.clone());
            }
        }
        Deriv::None => col.no_collation = true,
        Deriv::Indeterminate | Deriv::Conflicted => col.collation_conflict = true,
    }
}

/// The catalog `Type` whose `resolved_type_of_col` round-trips to `rt` — used to give a CTE's
/// synthetic columns a `Type` (spec/design/cte.md). An untyped NULL column maps to `text`
/// (PostgreSQL's unknown -> text rule). A decimal's per-column typmod is irrelevant for a read-only
/// CTE column (values flow through unchanged), so it is dropped. An anonymous `ROW(...)` composite
/// becomes a `Type::Record` carrying its field list (spec/design/composite.md §1).
pub(crate) fn type_from_resolved(rt: &ResolvedType) -> Result<Type> {
    Ok(match rt {
        ResolvedType::Int(s) | ResolvedType::Float(s) => Type::Scalar(*s),
        ResolvedType::Bool => Type::Scalar(ScalarType::Bool),
        ResolvedType::Text | ResolvedType::Null => Type::Scalar(ScalarType::Text),
        ResolvedType::Decimal => Type::Scalar(ScalarType::Decimal),
        ResolvedType::Bytea => Type::Scalar(ScalarType::Bytea),
        ResolvedType::Uuid => Type::Scalar(ScalarType::Uuid),
        ResolvedType::Json => Type::Scalar(ScalarType::Json),
        ResolvedType::JsonPath => Type::Scalar(ScalarType::JsonPath),
        ResolvedType::Jsonb => Type::Scalar(ScalarType::Jsonb),
        ResolvedType::Timestamp => Type::Scalar(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Type::Scalar(ScalarType::Timestamptz),
        ResolvedType::Date => Type::Scalar(ScalarType::Date),
        ResolvedType::Interval => Type::Scalar(ScalarType::Interval),
        ResolvedType::Composite(r) => match &r.name {
            Some(n) => Type::Composite(crate::types::CompositeRef { name: n.clone() }),
            // An anonymous `ROW(…)` column carries its shape inline (composite.md §1).
            None => Type::Record(
                r.fields
                    .iter()
                    .map(|(name, ty)| Ok((name.clone(), type_from_resolved(ty)?)))
                    .collect::<Result<_>>()?,
            ),
        },
        ResolvedType::Array(elem) => Type::Array(Box::new(type_from_resolved(elem)?)),
        ResolvedType::Range(elem) => Type::Range(Box::new(type_from_resolved(elem)?)),
    })
}

/// The scalar element type of a resolved range element (`ResolvedType::Range(elem)`'s `elem`). A
/// range's element is always one of the six scalar subtypes; `None` for anything else (which never
/// occurs for a valid range). Used to name a range (`i32` → `i32range`) and to build its codec.
pub(crate) fn resolved_range_element_scalar(elem: &ResolvedType) -> Option<ScalarType> {
    match elem {
        ResolvedType::Int(s) => Some(*s),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
        ResolvedType::Date => Some(ScalarType::Date),
        _ => None,
    }
}
