//! Structural rules for stored expressions: what a CHECK constraint, a column DEFAULT, an index
//! key expression and a partial-index predicate may contain (constraints.md, indexes.md), plus the
//! column-reference and parameter scans those rules and DDL share. Pure AST walks — nothing here
//! resolves or evaluates.

use super::*;

/// The structural CHECK-expression rejections (spec/design/constraints.md §4.1), applied in
/// a single depth-first pre-order walk before resolution: a subquery is 0A000, an aggregate
/// call 42803, a bind parameter 42P02 — PG's codes and messages (oracle-probed; PG
/// interleaves these with resolution in parse order, a documented micro-order divergence).
pub(crate) fn reject_check_structure(e: &Expr) -> Result<()> {
    match e {
        Expr::ScalarSubquery(_, _)
        | Expr::Exists(_, _)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in check constraint",
        )),
        Expr::Param(n, _) => Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        )),
        Expr::FuncCall { name, args, .. } => {
            if is_aggregate_name(name) {
                return Err(EngineError::new(
                    SqlState::GroupingError,
                    "aggregate functions are not allowed in check constraints",
                ));
            }
            args.iter().try_for_each(reject_check_structure)
        }
        Expr::Column(_, _)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_, _)
        | Expr::TypedLiteral { .. } => Ok(()),
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => {
            reject_check_structure(inner)
        }
        Expr::Collate { inner, .. } => reject_check_structure(inner),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => reject_check_structure(operand),
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => {
            reject_check_structure(ctx)?;
            reject_check_structure(path)?;
            for p in passing {
                reject_check_structure(&p.expr)?;
            }
            Ok(())
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(rhs)
        }
        Expr::In { lhs, list, .. } => {
            reject_check_structure(lhs)?;
            list.iter().try_for_each(reject_check_structure)
        }
        Expr::Quantified { lhs, array, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(array)
        }
        Expr::Row(items, _) | Expr::Array(items, _) => {
            items.iter().try_for_each(reject_check_structure)
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => reject_check_structure(base),
        // `t.*` cannot syntactically reach a CHECK expression (it is a select-item-only shape —
        // `CHECK (t.*)` is a 42601 in the parser); accept it structurally for exhaustiveness.
        Expr::QualifiedStar { .. } => Ok(()),
        Expr::Subscript { base, subscripts } => {
            reject_check_structure(base)?;
            subscripts
                .iter()
                .flat_map(subscript_spec_exprs)
                .try_for_each(reject_check_structure)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            reject_check_structure(lhs)?;
            reject_check_structure(lo)?;
            reject_check_structure(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => {
            if let Some(op) = operand {
                reject_check_structure(op)?;
            }
            for (c, r) in whens {
                reject_check_structure(c)?;
                reject_check_structure(r)?;
            }
            match els {
                Some(e) => reject_check_structure(e),
                None => Ok(()),
            }
        }
        Expr::Coalesce(args, _) => {
            for a in args {
                reject_check_structure(a)?;
            }
            Ok(())
        }
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
            for a in args {
                reject_check_structure(a)?;
            }
            Ok(())
        }
    }
}

/// The structural rejections for a `DEFAULT` expression (constraints.md §2), a single
/// depth-first pre-walk run before name/type resolution (the same micro-order divergence from
/// PG that `reject_check_structure` carries). A default extends the CHECK rejections with one
/// more: it may **not reference a column** (it is computed before the row exists). Codes match
/// PostgreSQL (oracle-probed): a column reference / subquery is `0A000`, an aggregate `42803`,
/// a bind parameter `42P02`.
pub(crate) fn reject_default_structure(e: &Expr) -> Result<()> {
    match e {
        Expr::Column(_, _) | Expr::QualifiedColumn { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use column reference in DEFAULT expression",
        )),
        Expr::ScalarSubquery(_, _)
        | Expr::Exists(_, _)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in DEFAULT expression",
        )),
        Expr::Param(n, _) => Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        )),
        Expr::FuncCall { name, args, .. } => {
            if is_aggregate_name(name) {
                return Err(EngineError::new(
                    SqlState::GroupingError,
                    "aggregate functions are not allowed in DEFAULT expressions",
                ));
            }
            args.iter().try_for_each(reject_default_structure)
        }
        Expr::Literal(_, _) | Expr::TypedLiteral { .. } => Ok(()),
        Expr::Cast { inner, .. } | Expr::Extract { source: inner, .. } => {
            reject_default_structure(inner)
        }
        Expr::Collate { inner, .. } => reject_default_structure(inner),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => reject_default_structure(operand),
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => {
            reject_default_structure(ctx)?;
            reject_default_structure(path)?;
            for p in passing {
                reject_default_structure(&p.expr)?;
            }
            Ok(())
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(rhs)
        }
        Expr::In { lhs, list, .. } => {
            reject_default_structure(lhs)?;
            list.iter().try_for_each(reject_default_structure)
        }
        Expr::Quantified { lhs, array, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(array)
        }
        Expr::Row(items, _) | Expr::Array(items, _) => {
            items.iter().try_for_each(reject_default_structure)
        }
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => reject_default_structure(base),
        // `t.*` cannot syntactically reach a DEFAULT expression (select-item-only); accept
        // structurally for exhaustiveness.
        Expr::QualifiedStar { .. } => Ok(()),
        Expr::Subscript { base, subscripts } => {
            reject_default_structure(base)?;
            subscripts
                .iter()
                .flat_map(subscript_spec_exprs)
                .try_for_each(reject_default_structure)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            reject_default_structure(lhs)?;
            reject_default_structure(lo)?;
            reject_default_structure(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => {
            if let Some(op) = operand {
                reject_default_structure(op)?;
            }
            for (c, r) in whens {
                reject_default_structure(c)?;
                reject_default_structure(r)?;
            }
            match els {
                Some(e) => reject_default_structure(e),
                None => Ok(()),
            }
        }
        Expr::Coalesce(args, _) => {
            for a in args {
                reject_default_structure(a)?;
            }
            Ok(())
        }
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
            for a in args {
                reject_default_structure(a)?;
            }
            Ok(())
        }
    }
}

/// The distinct columns a CHECK expression references, as indices into `columns` — the input
/// to PG's auto-naming rule (constraints.md §4.3: exactly one distinct column →
/// `<table>_<col>_check`). Resolution already validated every reference, so an unknown name
/// is simply skipped; a qualified reference counts its column like a bare one (oracle-probed).
pub(crate) fn check_referenced_columns(e: &Expr, columns: &[Column]) -> Vec<usize> {
    pub(crate) fn walk(e: &Expr, columns: &[Column], out: &mut Vec<usize>) {
        let mut note = |name: &str| {
            if let Some(i) = columns.iter().position(|c| c.name == name)
                && !out.contains(&i)
            {
                out.push(i);
            }
        };
        match e {
            Expr::Column(name, _) | Expr::QualifiedColumn { name, .. } => note(name),
            Expr::Literal(_, _) | Expr::TypedLiteral { .. } | Expr::Param(_, _) => {}
            Expr::Cast { inner, .. }
            | Expr::Collate { inner, .. }
            | Expr::Extract { source: inner, .. } => walk(inner, columns, out),
            Expr::Unary { operand, .. }
            | Expr::IsNull { operand, .. }
            | Expr::IsJson { operand, .. }
            | Expr::JsonCtor { operand, .. } => walk(operand, columns, out),
            Expr::JsonExists {
                ctx, path, passing, ..
            }
            | Expr::JsonValue {
                ctx, path, passing, ..
            }
            | Expr::JsonQuery {
                ctx, path, passing, ..
            } => {
                walk(ctx, columns, out);
                walk(path, columns, out);
                for p in passing {
                    walk(&p.expr, columns, out);
                }
            }
            Expr::Binary { lhs, rhs, .. }
            | Expr::IsDistinctFrom { lhs, rhs, .. }
            | Expr::Like { lhs, rhs, .. }
            | Expr::Regex { lhs, rhs, .. } => {
                walk(lhs, columns, out);
                walk(rhs, columns, out);
            }
            Expr::In { lhs, list, .. } => {
                walk(lhs, columns, out);
                for x in list {
                    walk(x, columns, out);
                }
            }
            Expr::Quantified { lhs, array, .. } => {
                walk(lhs, columns, out);
                walk(array, columns, out);
            }
            Expr::Between { lhs, lo, hi, .. } => {
                walk(lhs, columns, out);
                walk(lo, columns, out);
                walk(hi, columns, out);
            }
            Expr::Case {
                operand,
                whens,
                els,
                ..
            } => {
                if let Some(op) = operand {
                    walk(op, columns, out);
                }
                for (c, r) in whens {
                    walk(c, columns, out);
                    walk(r, columns, out);
                }
                if let Some(e) = els {
                    walk(e, columns, out);
                }
            }
            Expr::Coalesce(args, _) => {
                for a in args {
                    walk(a, columns, out);
                }
            }
            Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
                for a in args {
                    walk(a, columns, out);
                }
            }
            Expr::FuncCall { args, .. } => {
                for a in args {
                    walk(a, columns, out);
                }
            }
            Expr::Row(items, _) | Expr::Array(items, _) => {
                for it in items {
                    walk(it, columns, out);
                }
            }
            Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => walk(base, columns, out),
            // `t.*` cannot appear in a CHECK expression (select-item-only); no columns to note.
            Expr::QualifiedStar { .. } => {}
            Expr::Subscript { base, subscripts } => {
                walk(base, columns, out);
                for e in subscripts.iter().flat_map(subscript_spec_exprs) {
                    walk(e, columns, out);
                }
            }
            // Unreachable in a validated check (rejected by `reject_check_structure`).
            Expr::ScalarSubquery(_, _)
            | Expr::Exists(_, _)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. } => {}
        }
    }
    let mut out = Vec::new();
    walk(e, columns, &mut out);
    out
}

/// The structural rejections for a PARTIAL-index predicate (spec/design/indexes.md §9), applied
/// before resolution: a **subquery** is `0A000` (`cannot use subquery in index predicate`) and a
/// **bind parameter** `$N` is `42P02` (`there is no parameter $N`) — both admitted by the ordinary
/// resolver, so they are caught here (the aggregate `42803` / window `42P20` / non-boolean `42804`
/// rejections then fall out of the `Forbidden`-context boolean resolve). Reuses
/// [`index_expr_has_subquery`] for the subquery walk, then finds the first param.
pub(crate) fn reject_index_predicate_structure(e: &Expr) -> Result<()> {
    if index_expr_has_subquery(e) {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in index predicate",
        ));
    }
    if let Some(n) = index_expr_first_param(e) {
        return Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        ));
    }
    Ok(())
}

/// The 1-based index of the first bind parameter `$N` in an expression, or `None` if it has none
/// (used by [`reject_index_predicate_structure`]). A depth-first pre-order walk mirroring
/// [`index_expr_has_subquery`]'s traversal.
pub(crate) fn index_expr_first_param(e: &Expr) -> Option<u32> {
    match e {
        Expr::Param(n, _) => Some(*n),
        Expr::Column(_, _)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_, _)
        | Expr::TypedLiteral { .. }
        | Expr::QualifiedStar { .. } => None,
        Expr::ScalarSubquery(_, _)
        | Expr::Exists(_, _)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => None,
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::IsNull { operand: inner, .. }
        | Expr::IsJson { operand: inner, .. }
        | Expr::JsonCtor { operand: inner, .. }
        | Expr::FieldAccess { base: inner, .. }
        | Expr::FieldStar { base: inner } => index_expr_first_param(inner),
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => index_expr_first_param(ctx)
            .or_else(|| index_expr_first_param(path))
            .or_else(|| passing.iter().find_map(|p| index_expr_first_param(&p.expr))),
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            index_expr_first_param(lhs).or_else(|| index_expr_first_param(rhs))
        }
        Expr::In { lhs, list, .. } => {
            index_expr_first_param(lhs).or_else(|| list.iter().find_map(index_expr_first_param))
        }
        Expr::Quantified { lhs, array, .. } => {
            index_expr_first_param(lhs).or_else(|| index_expr_first_param(array))
        }
        Expr::Between { lhs, lo, hi, .. } => index_expr_first_param(lhs)
            .or_else(|| index_expr_first_param(lo))
            .or_else(|| index_expr_first_param(hi)),
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => operand
            .as_deref()
            .and_then(index_expr_first_param)
            .or_else(|| {
                whens.iter().find_map(|(c, r)| {
                    index_expr_first_param(c).or_else(|| index_expr_first_param(r))
                })
            })
            .or_else(|| els.as_deref().and_then(index_expr_first_param)),
        Expr::Coalesce(args, _) => args.iter().find_map(index_expr_first_param),
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
            args.iter().find_map(index_expr_first_param)
        }
        Expr::FuncCall { args, .. } => args.iter().find_map(index_expr_first_param),
        Expr::Row(items, _) | Expr::Array(items, _) => {
            items.iter().find_map(index_expr_first_param)
        }
        Expr::Subscript { base, subscripts } => index_expr_first_param(base).or_else(|| {
            subscripts
                .iter()
                .flat_map(subscript_spec_exprs)
                .find_map(index_expr_first_param)
        }),
    }
}

/// Whether an index-key expression contains a SUBQUERY (spec/design/indexes.md §2): a scalar
/// subquery, `EXISTS`, `IN (subquery)`, or a quantified subquery. A subquery reads other rows, so
/// it is not a deterministic function of this row — `0A000` at CREATE INDEX (PostgreSQL: "cannot
/// use subquery in index expression"). Unlike an aggregate/window (rejected by resolution), the
/// resolver admits an uncorrelated subquery, so it is caught here.
pub(crate) fn index_expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_, _)
        | Expr::Exists(_, _)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => true,
        Expr::Column(_, _)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_, _)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_, _)
        | Expr::QualifiedStar { .. } => false,
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. }
        | Expr::Unary { operand: inner, .. }
        | Expr::IsNull { operand: inner, .. }
        | Expr::IsJson { operand: inner, .. }
        | Expr::JsonCtor { operand: inner, .. }
        | Expr::FieldAccess { base: inner, .. }
        | Expr::FieldStar { base: inner } => index_expr_has_subquery(inner),
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => {
            index_expr_has_subquery(ctx)
                || index_expr_has_subquery(path)
                || passing.iter().any(|p| index_expr_has_subquery(&p.expr))
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => {
            index_expr_has_subquery(lhs) || index_expr_has_subquery(rhs)
        }
        Expr::In { lhs, list, .. } => {
            index_expr_has_subquery(lhs) || list.iter().any(index_expr_has_subquery)
        }
        Expr::Quantified { lhs, array, .. } => {
            index_expr_has_subquery(lhs) || index_expr_has_subquery(array)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            index_expr_has_subquery(lhs)
                || index_expr_has_subquery(lo)
                || index_expr_has_subquery(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => {
            operand.as_deref().is_some_and(index_expr_has_subquery)
                || whens
                    .iter()
                    .any(|(c, r)| index_expr_has_subquery(c) || index_expr_has_subquery(r))
                || els.as_deref().is_some_and(index_expr_has_subquery)
        }
        Expr::Coalesce(args, _) => args.iter().any(index_expr_has_subquery),
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => {
            args.iter().any(index_expr_has_subquery)
        }
        Expr::FuncCall { args, .. } => args.iter().any(index_expr_has_subquery),
        Expr::Row(items, _) | Expr::Array(items, _) => items.iter().any(index_expr_has_subquery),
        Expr::Subscript { base, subscripts } => {
            index_expr_has_subquery(base)
                || subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .any(index_expr_has_subquery)
        }
    }
}

/// The auto-name part for one index key element (spec/design/indexes.md §2, PG's
/// `ChooseIndexColumnNames`): a column key contributes its (exact) column name; a
/// bare-function-call expression its function name (`lower(email)` → `lower`); any other
/// expression the literal `expr`.
pub(crate) fn index_name_part(elem: &crate::ast::IndexKeyElem) -> String {
    use crate::ast::IndexKeyElem;
    match elem {
        IndexKeyElem::Column(name) => name.to_string(),
        IndexKeyElem::Expr {
            expr: Expr::FuncCall { name, .. },
            ..
        } => name.to_string(),
        IndexKeyElem::Expr { .. } => "expr".to_string(),
    }
}
