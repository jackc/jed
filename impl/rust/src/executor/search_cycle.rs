//! A recursive CTE's `SEARCH` / `CYCLE` clauses (spec/design/recursive-cte.md §9).
//!
//! The clauses add columns to the CTE: `SEARCH` an ordering column, `CYCLE` a cycle mark and a
//! path. PostgreSQL defines them by a rewrite of the CTE (rewriteSearchCycle.c); jed computes the
//! same columns from each term's produced rows. The planner resolves two lists of expressions over
//! a synthetic row — one over an anchor row (the user columns), one over a recursive-term row (the
//! user columns, then the parent row's added columns, which the recursive term carries as hidden
//! trailing outputs) — plus `CYCLE`'s filter, and the executor evaluates them per row.

use super::*;
use crate::ast::CycleClause;

/// The planned `SEARCH` / `CYCLE` columns of a recursive CTE (recursive-cte.md §9). Every
/// expression is resolved against a synthetic row: `anchor` over an anchor row's `user_cols`
/// values; `filter` and `recursive` over a recursive-term row — its `user_cols` values (coerced to
/// the anchor's types) followed by the parent row's added columns.
pub(crate) struct SearchCyclePlan {
    /// The CTE's own column count; the added columns follow them.
    pub(crate) user_cols: usize,
    /// The added columns' names and types, in order: the search column, then the cycle mark and
    /// the cycle path.
    pub(crate) names: Vec<String>,
    pub(crate) types: Vec<ResolvedType>,
    /// The added columns' values for an anchor row.
    pub(crate) anchor: Vec<RExpr>,
    /// The added columns' values for a recursive-term row.
    pub(crate) recursive: Vec<RExpr>,
    /// `CYCLE`'s `mark <> value` over the parent's mark: a recursive-term row is kept only when it
    /// is TRUE, so a row whose parent closed a cycle is not expanded.
    pub(crate) filter: Option<RExpr>,
}

/// A `CYCLE` clause's mark constants, typed (recursive-cte.md §9): `value` marks a row that closes
/// a cycle, `default` every other row. Both have the common type `ty`.
pub(crate) struct CycleMark {
    value: Expr,
    default: Expr,
    ty: ResolvedType,
    default_ty: ResolvedType,
}

/// The synthetic column name of user column `i` in an augment row. Never visible: the augment
/// expressions build anonymous rows, whose fields are `f1…fN` whatever the columns are called.
fn user_col(i: usize) -> Expr {
    Expr::Column(format!("*{i}*"), SrcPos::NONE)
}

/// The synthetic column name of added column `k` (search, mark, path) in a recursive augment row.
fn added_col(k: usize) -> Expr {
    Expr::Column(format!("*added{k}*"), SrcPos::NONE)
}

fn row_of(cols: &[usize]) -> Expr {
    Expr::Row(cols.iter().map(|&i| user_col(i)).collect(), SrcPos::NONE)
}

fn int_lit(n: i64) -> Expr {
    Expr::Literal(Literal::Int(n), SrcPos::NONE)
}

fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        pos: SrcPos::NONE,
    }
}

impl Engine {
    /// Type a `CYCLE` clause's mark constants (recursive-cte.md §9), before the CTE's body is planned
    /// (PostgreSQL's order). Omitted, they are `TRUE` / `FALSE`. A bare string literal takes the
    /// other constant's type, read through that type's input (`TO 1 DEFAULT 'N'` is `22P02`, as in
    /// PostgreSQL); the two must then have a common type (`42804`), and that type an equality
    /// operator (`42883`).
    pub(crate) fn resolve_cycle_mark(
        &self,
        cycle: &CycleClause,
        ptypes: &mut ParamTypes,
    ) -> Result<CycleMark> {
        let (mut value, mut default) = match (&cycle.mark_value, &cycle.mark_default) {
            (Some(v), Some(d)) => (v.clone(), d.clone()),
            _ => (
                Expr::Literal(Literal::Bool(true), SrcPos::NONE),
                Expr::Literal(Literal::Bool(false), SrcPos::NONE),
            ),
        };
        let empty = cte_synthetic_table_cols("*cycle*", &[], &[], None, None, None)?;
        let scope = Scope::single(self, &empty);
        let type_of = |e: &Expr, ptypes: &mut ParamTypes| -> Result<ResolvedType> {
            Ok(resolve(&scope, e, None, &mut AggCtx::Forbidden, ptypes)?.1)
        };
        let vt = type_of(&value, ptypes)?;
        let dt = type_of(&default, ptypes)?;
        // A bare string literal is PostgreSQL's `unknown`: it takes the other constant's type.
        let adapt = |e: &mut Expr, other: &ResolvedType| {
            if let (Expr::Literal(Literal::Text(s), pos), Some(st)) = (&*e, elem_scalar_hint(other))
            {
                if !st.is_text() {
                    *e = Expr::TypedLiteral {
                        type_name: st.canonical_name().to_string(),
                        text: s.clone(),
                        pos: *pos,
                        type_pos: SrcPos::NONE,
                    };
                }
            }
        };
        adapt(&mut value, &dt);
        adapt(&mut default, &vt);
        let vt = type_of(&value, ptypes)?;
        let dt = type_of(&default, ptypes)?;
        let ty = match (&vt, &dt) {
            (ResolvedType::Null, ResolvedType::Null) => ResolvedType::Text,
            (ResolvedType::Null, t) | (t, ResolvedType::Null) => t.clone(),
            (a, b) => common_type(a, b).ok_or_else(|| {
                EngineError::new(
                    SqlState::DatatypeMismatch,
                    format!(
                        "CYCLE types {} and {} cannot be matched",
                        a.type_name(),
                        b.type_name()
                    ),
                )
                .with_position(cycle.pos.0)
            })?,
        };
        require_setop_equality(&ty)?;
        Ok(CycleMark {
            value,
            default,
            ty,
            default_ty: dt,
        })
    }

    /// Validate a recursive CTE's `SEARCH` / `CYCLE` clauses against its columns and resolve the
    /// added columns' expressions (recursive-cte.md §9). `user` is the CTE's relation (its columns
    /// named as the CTE names them) and `user_types` their resolved types.
    pub(crate) fn plan_search_cycle(
        &self,
        cte: &Cte,
        user: &Table,
        user_types: &[ResolvedType],
        mark: Option<&CycleMark>,
        ptypes: &mut ParamTypes,
    ) -> Result<SearchCyclePlan> {
        let names: Vec<&str> = user.columns.iter().map(|c| c.name.as_str()).collect();
        let search_cols = match &cte.search {
            Some(s) => Some(clause_columns(&s.columns, &names, "search", s.pos)?),
            None => None,
        };
        if let Some(s) = &cte.search {
            if names.contains(&s.set_column.as_str()) {
                return Err(clause_error(
                    format!(
                        "search sequence column name \"{}\" already used in WITH query column list",
                        s.set_column
                    ),
                    s.pos,
                ));
            }
        }
        let cycle_cols = match &cte.cycle {
            Some(c) => Some(clause_columns(&c.columns, &names, "cycle", c.pos)?),
            None => None,
        };
        if let Some(c) = &cte.cycle {
            for (col, what) in [(&c.mark_column, "mark"), (&c.path_column, "path")] {
                if names.contains(&col.as_str()) {
                    return Err(clause_error(
                        format!(
                            "cycle {what} column name \"{col}\" already used in WITH query column list"
                        ),
                        c.pos,
                    ));
                }
            }
            if c.mark_column == c.path_column {
                return Err(clause_error(
                    "cycle mark column name and cycle path column name are the same".to_string(),
                    c.pos,
                ));
            }
        }
        if let (Some(s), Some(c)) = (&cte.search, &cte.cycle) {
            for (col, what) in [(&c.mark_column, "mark"), (&c.path_column, "path")] {
                if s.set_column == *col {
                    return Err(clause_error(
                        format!(
                            "search sequence column name and cycle {what} column name are the same"
                        ),
                        s.pos,
                    ));
                }
            }
        }

        // The added columns' expressions, over the synthetic anchor row (`*i*` = user column i)
        // and recursive row (then `*addedK*` = the parent's added column K).
        let mut added_names = Vec::new();
        let mut anchor_exprs = Vec::new();
        let mut recursive_exprs = Vec::new();
        let mut filter = None;
        if let (Some(s), Some(cols)) = (&cte.search, &search_cols) {
            let k = added_names.len();
            added_names.push(s.set_column.clone());
            if s.breadth_first {
                // ROW(0, cols) / ROW(parent.f1 + 1, cols) — the depth, then the columns.
                let mut first = vec![int_lit(0)];
                first.extend(cols.iter().map(|&i| user_col(i)));
                anchor_exprs.push(Expr::Row(first, SrcPos::NONE));
                let depth = Expr::FieldAccess {
                    base: Box::new(added_col(k)),
                    field: "f1".to_string(),
                };
                let mut next = vec![binary(BinaryOp::Add, depth, int_lit(1))];
                next.extend(cols.iter().map(|&i| user_col(i)));
                recursive_exprs.push(Expr::Row(next, SrcPos::NONE));
            } else {
                // ARRAY[ROW(cols)] / parent || ARRAY[ROW(cols)] — the path of rows seen.
                anchor_exprs.push(Expr::Array(vec![row_of(cols)], SrcPos::NONE));
                recursive_exprs.push(binary(
                    BinaryOp::Concat,
                    added_col(k),
                    Expr::Array(vec![row_of(cols)], SrcPos::NONE),
                ));
            }
        }
        if let (Some(c), Some(cols), Some(mark)) = (&cte.cycle, &cycle_cols, mark) {
            let mark_k = added_names.len();
            added_names.push(c.mark_column.clone());
            let path_k = added_names.len();
            added_names.push(c.path_column.clone());
            // The anchor's mark is the default, cast to the mark type when it widens.
            let default = if mark.default_ty == mark.ty || mark.default_ty == ResolvedType::Null {
                mark.default.clone()
            } else {
                Expr::Cast {
                    inner: Box::new(mark.default.clone()),
                    type_name: mark.ty.type_name(),
                    type_mod: None,
                    pos: SrcPos::NONE,
                    cast_pos: SrcPos::NONE,
                    type_pos: SrcPos::NONE,
                }
            };
            anchor_exprs.push(default);
            anchor_exprs.push(Expr::Array(vec![row_of(cols)], SrcPos::NONE));
            // CASE WHEN ROW(cols) = ANY (parent path) THEN value ELSE default END.
            recursive_exprs.push(Expr::Case {
                operand: None,
                whens: vec![(
                    Expr::Quantified {
                        op: BinaryOp::Eq,
                        all: false,
                        lhs: Box::new(row_of(cols)),
                        array: Box::new(added_col(path_k)),
                        pos: SrcPos::NONE,
                    },
                    mark.value.clone(),
                )],
                els: Some(Box::new(mark.default.clone())),
                pos: SrcPos::NONE,
                when_pos: vec![SrcPos::NONE],
            });
            recursive_exprs.push(binary(
                BinaryOp::Concat,
                added_col(path_k),
                Expr::Array(vec![row_of(cols)], SrcPos::NONE),
            ));
            filter = Some(binary(BinaryOp::Ne, added_col(mark_k), mark.value.clone()));
        }

        // Resolve over the anchor row; its types are the added columns' types.
        let user_names: Vec<String> = (0..user_types.len()).map(|i| format!("*{i}*")).collect();
        let anchor_row =
            cte_synthetic_table_cols("*anchor*", &user_names, user_types, None, None, None)?;
        let anchor_scope = Scope::single(self, &anchor_row);
        let mut anchor = Vec::new();
        let mut types = Vec::new();
        for e in &anchor_exprs {
            let (r, t) = resolve(&anchor_scope, e, None, &mut AggCtx::Forbidden, ptypes)?;
            anchor.push(r);
            types.push(t);
        }
        // Resolve over the recursive row: the user columns, then the parent's added columns.
        let mut row_names = user_names.clone();
        row_names.extend((0..types.len()).map(|k| format!("*added{k}*")));
        let mut row_types = user_types.to_vec();
        row_types.extend(types.iter().cloned());
        let recursive_row =
            cte_synthetic_table_cols("*recursive*", &row_names, &row_types, None, None, None)?;
        let recursive_scope = Scope::single(self, &recursive_row);
        let mut recursive = Vec::new();
        for (e, t) in recursive_exprs.iter().zip(&types) {
            let (r, rt) = resolve(&recursive_scope, e, None, &mut AggCtx::Forbidden, ptypes)?;
            debug_assert!(
                &rt == t,
                "an added column's recursive type is its anchor type"
            );
            recursive.push(r);
        }
        let filter = match &filter {
            Some(f) => Some(resolve(&recursive_scope, f, None, &mut AggCtx::Forbidden, ptypes)?.0),
            None => None,
        };
        Ok(SearchCyclePlan {
            user_cols: user_types.len(),
            names: added_names,
            types,
            anchor,
            recursive,
            filter,
        })
    }

    /// Give each row its `SEARCH` / `CYCLE` columns (recursive-cte.md §9): evaluate `exprs` over the
    /// row and replace everything past the CTE's own columns with the results. With a `filter`, a
    /// row is dropped unless the filter is TRUE. Each row is a cost-ceiling check point; the
    /// expressions charge their ordinary evaluation cost, returned as the second value.
    pub(crate) fn search_cycle_rows(
        &self,
        plan: &SearchCyclePlan,
        exprs: &[RExpr],
        filter: Option<&RExpr>,
        rows: Vec<Row>,
        params: &[Value],
        outer: &[&[Value]],
    ) -> Result<(Vec<Row>, i64)> {
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params,
            outer,
            rng: &stmt_rng,
            ctes: CteCtx::empty(),
            host_batch: None,
        };
        let mut meter = self.session.new_meter();
        let acct = self.session.query_account();
        let before = acct.measure_rows(&rows);
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            meter.guard()?;
            if let Some(f) = filter {
                if f.eval(&row, &env, &mut meter)? != Value::Bool(true) {
                    continue;
                }
            }
            let mut added = Vec::with_capacity(exprs.len());
            for e in exprs {
                added.push(e.eval(&row, &env, &mut meter)?);
            }
            let mut row = row;
            row.truncate(plan.user_cols);
            row.extend(added);
            out.push(row);
        }
        let after = acct.measure_rows(&out);
        if after >= before {
            acct.reserve(after - before)?;
        } else {
            acct.release(before - after);
        }
        Ok((out, meter.accrued))
    }
}

/// How many trailing columns of `rel` `*` skips — a recursive CTE's SEARCH / CYCLE columns while
/// its own recursive term is planned (recursive-cte.md §9).
pub(crate) fn star_hidden_count(rel: &ScopeRel, ctes: &[&CteBinding]) -> usize {
    rel.cte.map_or(0, |ci| ctes[ci].star_hidden.get())
}

/// The recursive term with the self-reference's added columns appended as hidden trailing outputs
/// (`self.seq`, `self.mark`, `self.path` — recursive-cte.md §9). The self-reference is a direct
/// FROM / JOIN relation of the term's SELECT (`analyze_recursive_cte` verified it); its label is
/// its alias or the CTE's name.
pub(crate) fn with_carried_columns(term: &QueryExpr, cte: &str, added: &[String]) -> QueryExpr {
    let mut term = term.clone();
    let QueryExpr::Select(sel) = &mut term else {
        unreachable!("a SEARCH / CYCLE recursive term is a SELECT")
    };
    let label = sel
        .from
        .iter()
        .chain(sel.joins.iter().map(|j| &j.table))
        .find(|t| t.subquery.is_none() && t.args.is_none() && t.values.is_none() && t.name == cte)
        .map(|t| t.alias.clone().unwrap_or_else(|| t.name.clone()))
        .expect("the recursive term references the CTE in its FROM");
    sel.tail = added
        .iter()
        .map(|name| Expr::QualifiedColumn {
            qualifier: label.clone(),
            name: name.clone(),
            pos: SrcPos::NONE,
        })
        .collect();
    term
}

/// The CTE column positions a `SEARCH` / `CYCLE` column list names (recursive-cte.md §9): each must
/// be a column of the CTE (`42601`) and appear once (`42701`). A name the CTE has twice takes its
/// first column, as in PostgreSQL.
fn clause_columns(cols: &[String], names: &[&str], what: &str, pos: SrcPos) -> Result<Vec<usize>> {
    let mut out: Vec<usize> = Vec::with_capacity(cols.len());
    for (i, col) in cols.iter().enumerate() {
        let Some(at) = names.iter().position(|n| n == col) else {
            return Err(clause_error(
                format!("{what} column \"{col}\" not in WITH query column list"),
                pos,
            ));
        };
        if cols[..i].contains(col) {
            return Err(EngineError::new(
                SqlState::DuplicateColumn,
                format!("{what} column \"{col}\" specified more than once"),
            )
            .with_position(pos.0));
        }
        out.push(at);
    }
    Ok(out)
}

fn clause_error(message: String, pos: SrcPos) -> EngineError {
    EngineError::new(SqlState::SyntaxError, message).with_position(pos.0)
}
