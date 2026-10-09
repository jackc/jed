//! Logical rewrite rules — Stage 2 of the planner (spec/design/planner.md §3). The stage runs after
//! resolve has built the logical plan and before `optimize_select` chooses access paths. Its rules
//! are pure plan→plan transforms: they never execute anything and never read a parameter value.
//!
//! The WHERE rewrite owns two decisions, recorded in `plan.pushdown` and never by mutating
//! `plan.filter` (the complete WHERE stays the input every access-path detector and estimator rule
//! reads):
//!
//!   - contradiction: the WHERE's top-level AND-chain is provably never TRUE from plan-time
//!     literals alone, so no relation is read and the FROM produces no rows (planner.md §3.1);
//!   - pushdown: in a multi-relation SELECT, each structurally safe single-base-relation conjunct of
//!     a relation no outer join NULL-extends is evaluated as that relation's rows are read, and only
//!     the remaining conjuncts are re-applied to the joined rows (planner.md §3.2).
//!
//! Rust note: a resolved `RExpr` is not `Clone` (a subquery owns its plan), so the residual cannot
//! be an owned tree beside `filter`. It is recorded as the residual conjuncts' positions in the
//! flattened WHERE and borrowed as a [`FilterChain`] — the same nodes the Go core's residual shares
//! with its `filter`, so the uncorrelated-subquery fold reaches both. The pushed filters hold only
//! pushdown-safe nodes, so they are real (cloned) trees.

use super::*;

/// The stage-2 WHERE split. `rel_filters` / `rel_local` hold one entry per relation (`None`:
/// nothing pushed; both empty on a contradiction); `residual` lists the positions, in the flattened
/// top-level AND-chain of `filter`, of the conjuncts left for the joined rows (empty when every
/// conjunct was pushed).
pub(crate) struct WherePushdown {
    contradiction: bool,
    /// The pushed filters in the plan's global slot numbering (EXPLAIN, estimator).
    rel_filters: Vec<Option<RExpr>>,
    /// The same filters rebased to the relation's own row (execution).
    rel_local: Vec<Option<RExpr>>,
    residual: Vec<usize>,
}

/// A post-join WHERE: the left-deep AND, in source order, of `conjuncts`. A one-element chain is that
/// expression itself — the complete WHERE when nothing was pushed. Every operation below reproduces
/// exactly what the same operation does on the equivalent `RExpr::And` tree (eval order, guards and
/// charges, rendering, estimator recursion), so the chain is observably that tree.
pub(crate) struct FilterChain<'a> {
    pub(crate) conjuncts: Vec<&'a RExpr>,
}

impl FilterChain<'_> {
    /// Evaluate the left-deep AND: each AND node guards and charges on entry (outermost first), then
    /// the conjuncts evaluate in source order — `RExpr::And`'s eval, unrolled.
    pub(crate) fn eval(&self, row: &[Value], env: &EvalEnv, m: &mut Meter) -> Result<Value> {
        for _ in 1..self.conjuncts.len() {
            m.guard()?;
            m.charge(operator_cost("and"));
        }
        let mut acc = self.conjuncts[0].eval(row, env, m)?;
        for c in &self.conjuncts[1..] {
            let v = c.eval(row, env, m)?;
            acc = and3(&acc, &v);
        }
        Ok(acc)
    }

    /// `render_rexpr` of the left-deep AND.
    pub(crate) fn render(&self) -> String {
        let mut s = render_rexpr(self.conjuncts[0]);
        for c in &self.conjuncts[1..] {
            s = format!("({s} and {})", render_rexpr(c));
        }
        s
    }

    /// `conjunct_count` of the left-deep AND.
    pub(crate) fn conjunct_count(&self) -> i64 {
        self.conjuncts.iter().map(|c| conjunct_count(c)).sum()
    }

    /// `estimator_operator_nodes` of the left-deep AND (each AND node counts one).
    pub(crate) fn operator_nodes(&self) -> i64 {
        use crate::estimator::sat_add;
        let mut nodes = estimator_operator_nodes(Some(self.conjuncts[0]));
        for c in &self.conjuncts[1..] {
            nodes = sat_add(1, sat_add(nodes, estimator_operator_nodes(Some(c))));
        }
        nodes
    }
}

impl SelectPlan {
    /// The WHERE that remains to be evaluated over the joined rows: the residual after a pushdown,
    /// otherwise the complete WHERE. A contradiction never reaches it (no row is produced).
    pub(crate) fn post_join_filter(&self) -> Option<FilterChain<'_>> {
        let filter = self.filter.as_ref()?;
        match &self.pushdown {
            Some(pd) if !pd.contradiction => {
                if pd.residual.is_empty() {
                    return None;
                }
                let mut all = Vec::new();
                estimator_flatten_boolean(filter, true, &mut all);
                Some(FilterChain {
                    conjuncts: pd.residual.iter().map(|&i| all[i]).collect(),
                })
            }
            _ => Some(FilterChain {
                conjuncts: vec![filter],
            }),
        }
    }

    /// Whether [`SelectPlan::post_join_filter`] is present (without building the chain).
    pub(crate) fn has_post_join_filter(&self) -> bool {
        match &self.pushdown {
            Some(pd) if !pd.contradiction => !pd.residual.is_empty(),
            _ => self.filter.is_some(),
        }
    }

    /// Stage 2 proved the WHERE never TRUE (planner.md §3.1).
    pub(crate) fn where_contradiction(&self) -> bool {
        self.pushdown.as_ref().is_some_and(|pd| pd.contradiction)
    }

    /// Relation `ri`'s pushed filter in global slots (EXPLAIN, estimator), or `None`.
    pub(crate) fn pushed_filter(&self, ri: usize) -> Option<&RExpr> {
        let pd = self.pushdown.as_ref()?;
        if pd.contradiction {
            return None;
        }
        pd.rel_filters.get(ri)?.as_ref()
    }

    /// Relation `ri`'s pushed filter rebased to the relation's own row (execution), or `None`.
    pub(crate) fn pushed_local_filter(&self, ri: usize) -> Option<&RExpr> {
        let pd = self.pushdown.as_ref()?;
        if pd.contradiction {
            return None;
        }
        pd.rel_local.get(ri)?.as_ref()
    }
}

/// The stage-2 driver: contradiction detection first (it subsumes pushdown), then WHERE pushdown. A
/// plan without a WHERE is untouched.
pub(crate) fn rewrite_where(plan: &mut SelectPlan) {
    let Some(filter) = plan.filter.as_ref() else {
        return;
    };
    let mut conjuncts = Vec::new();
    estimator_flatten_boolean(filter, true, &mut conjuncts);
    if where_conjuncts_contradict(&conjuncts) {
        plan.pushdown = Some(WherePushdown {
            contradiction: true,
            rel_filters: Vec::new(),
            rel_local: Vec::new(),
            residual: Vec::new(),
        });
        return;
    }
    if plan.rels.len() < 2 {
        return;
    }
    let nullable = nullable_relations(plan);
    let mut owners: Vec<Option<usize>> = Vec::with_capacity(conjuncts.len());
    for c in &conjuncts {
        let owner = conjunct_single_relation(plan, c)
            .filter(|&ri| !nullable[ri] && pushdown_target(&plan.rels[ri]) && pushdown_safe(c));
        owners.push(owner);
    }
    if owners.iter().all(Option::is_none) {
        return;
    }
    let mut rel_filters: Vec<Option<RExpr>> = (0..plan.rels.len()).map(|_| None).collect();
    let mut residual = Vec::new();
    for (i, c) in conjuncts.iter().enumerate() {
        match owners[i] {
            Some(ri) => {
                let acc = rel_filters[ri].take();
                rel_filters[ri] = Some(and_conjunct(acc, clone_pushdown_safe(c)));
            }
            None => residual.push(i),
        }
    }
    let rel_local = rel_filters
        .iter()
        .enumerate()
        .map(|(ri, f)| f.as_ref().map(|f| rebase_columns(f, plan.rels[ri].offset)))
        .collect();
    plan.pushdown = Some(WherePushdown {
        contradiction: false,
        rel_filters,
        rel_local,
        residual,
    });
}

/// The plan-time contradiction proof (planner.md §3.1): a literal FALSE or NULL conjunct, a
/// comparison between two literals that is never TRUE, or the estimator's same-operand literal
/// contradiction inventory (`x op NULL`, conflicting equalities, an empty range) restricted to
/// comparisons whose operand is a bare column — a column has one value per row, while a
/// structurally equal volatile expression (`random() > 0.9 AND random() < 0.1`) does not. A missed
/// proof only forgoes the optimization; a false proof would drop rows, so every rule is sound under
/// three-valued logic.
fn where_conjuncts_contradict(conjuncts: &[&RExpr]) -> bool {
    let mut column_comparisons: Vec<&RExpr> = Vec::new();
    for &c in conjuncts {
        match c {
            RExpr::ConstNull | RExpr::ConstBool(false) => return true,
            RExpr::Compare { .. } => {
                if literal_comparison_false(c) {
                    return true;
                }
                if estimator_comparison_parts(c)
                    .is_some_and(|comparison| matches!(comparison.operand, RExpr::Column(_)))
                {
                    column_comparisons.push(c);
                }
            }
            _ => {}
        }
    }
    estimator_conjunction_contradictory(&column_comparisons)
}

/// Prove a comparison between two plan-time literals never TRUE. Text keeps the estimator's
/// byte-identity restriction to equality (ordering may use a collation).
fn literal_comparison_false(c: &RExpr) -> bool {
    let RExpr::Compare { op, lhs, rhs, .. } = c else {
        return false;
    };
    if !matches!(
        op,
        CmpOp::Eq | CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge
    ) {
        return false;
    }
    if !estimator_literal(lhs) || !estimator_literal(rhs) {
        return false;
    }
    if matches!(lhs.as_ref(), RExpr::ConstNull) || matches!(rhs.as_ref(), RExpr::ConstNull) {
        return true;
    }
    if (matches!(lhs.as_ref(), RExpr::ConstText(_)) || matches!(rhs.as_ref(), RExpr::ConstText(_)))
        && *op != CmpOp::Eq
    {
        return false;
    }
    estimator_literal_cmp(lhs, rhs).is_some_and(|order| !estimator_comparison_satisfied(order, *op))
}

/// Mark every relation an outer join NULL-extends in the left-deep FROM: `joins[k]` attaches
/// `rels[k+1]` to the accumulated `rels[0..=k]`.
fn nullable_relations(plan: &SelectPlan) -> Vec<bool> {
    let mut nullable = vec![false; plan.rels.len()];
    for (k, j) in plan.joins.iter().enumerate() {
        if matches!(j.kind, JoinKind::Left | JoinKind::Full) {
            nullable[k + 1] = true;
        }
        if matches!(j.kind, JoinKind::Right | JoinKind::Full) {
            for n in nullable.iter_mut().take(k + 1) {
                *n = true;
            }
        }
    }
    nullable
}

/// Admit only a non-lateral base table: its rows are read by an access path, so the pushed filter
/// runs exactly where the scan admits each row.
fn pushdown_target(rel: &PlanRel) -> bool {
    rel.srf.is_none() && rel.cte.is_none() && rel.derived.is_none() && !rel.lateral
}

/// The one relation whose columns `c` references; `None` when it references none or several. Only
/// the pushdown-safe node kinds are walked: any other node makes the conjunct ineligible anyway
/// (`pushdown_safe`), so it reports `None` without deciding an owner.
fn conjunct_single_relation(plan: &SelectPlan, c: &RExpr) -> Option<usize> {
    fn walk(plan: &SelectPlan, e: &RExpr, owner: &mut Option<usize>) -> bool {
        match e {
            RExpr::Column(slot) => {
                let Some(ri) = relation_of_slot(plan, *slot) else {
                    return false;
                };
                if owner.is_some_and(|o| o != ri) {
                    return false;
                }
                *owner = Some(ri);
                true
            }
            RExpr::And(l, r) | RExpr::Or(l, r) => walk(plan, l, owner) && walk(plan, r, owner),
            RExpr::Compare { lhs, rhs, .. } | RExpr::Distinct { lhs, rhs, .. } => {
                walk(plan, lhs, owner) && walk(plan, rhs, owner)
            }
            RExpr::Not(operand) | RExpr::IsNull { operand, .. } => walk(plan, operand, owner),
            RExpr::Param(_) => true,
            _ => estimator_literal(e),
        }
    }
    let mut owner = None;
    if walk(plan, c, &mut owner) {
        owner
    } else {
        None
    }
}

fn relation_of_slot(plan: &SelectPlan, slot: usize) -> Option<usize> {
    plan.rels
        .iter()
        .position(|rel| slot >= rel.offset && slot < rel.offset + rel.col_count)
}

/// The structural safety gate (planner.md §3.2): the conjunct must be unable to trap and must read
/// nothing but the row, constants and parameters — so evaluating it on a row the join would never
/// have produced can raise no error a non-pushed plan would not, and its value is the same wherever
/// it runs. The admitted shapes are boolean combinations (AND/OR/NOT) of comparisons, IS [NOT] NULL,
/// and IS [NOT] DISTINCT FROM over bare columns, literals and parameters, plus a bare boolean
/// column. A collated ordering comparison is excluded (its sort-key kernel can fail on unloaded
/// data); every other node kind — casts, arithmetic, functions, subqueries, outer references —
/// stays in the residual.
fn pushdown_safe(e: &RExpr) -> bool {
    match e {
        RExpr::And(l, r) | RExpr::Or(l, r) => pushdown_safe(l) && pushdown_safe(r),
        RExpr::Not(operand) => pushdown_safe(operand),
        RExpr::Column(_) => true,
        RExpr::Compare {
            op,
            lhs,
            rhs,
            collation,
        } => {
            if collation.is_some() && !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                return false;
            }
            pushdown_leaf(lhs) && pushdown_leaf(rhs)
        }
        RExpr::Distinct { lhs, rhs, .. } => pushdown_leaf(lhs) && pushdown_leaf(rhs),
        RExpr::IsNull { operand, .. } => pushdown_leaf(operand),
        _ => false,
    }
}

fn pushdown_leaf(e: &RExpr) -> bool {
    matches!(e, RExpr::Column(_) | RExpr::Param(_)) || estimator_literal(e)
}

/// Append `c` to a left-deep AND chain (source order is preserved by the caller).
fn and_conjunct(acc: Option<RExpr>, c: RExpr) -> RExpr {
    match acc {
        None => c,
        Some(acc) => RExpr::And(Box::new(acc), Box::new(c)),
    }
}

/// Clone a pushdown-safe tree (only the `pushdown_safe` node kinds and the `estimator_literal`
/// leaves reach it).
fn clone_pushdown_safe(e: &RExpr) -> RExpr {
    rebase_columns(e, 0)
}

/// Clone a pushdown-safe tree with every column slot shifted down by `offset`, so it evaluates
/// against the relation's own row. Only the `pushdown_safe` node kinds reach it.
fn rebase_columns(e: &RExpr, offset: usize) -> RExpr {
    let boxed = |e: &RExpr| Box::new(rebase_columns(e, offset));
    match e {
        RExpr::Column(i) => RExpr::Column(i - offset),
        RExpr::Param(i) => RExpr::Param(*i),
        RExpr::And(l, r) => RExpr::And(boxed(l), boxed(r)),
        RExpr::Or(l, r) => RExpr::Or(boxed(l), boxed(r)),
        RExpr::Not(operand) => RExpr::Not(boxed(operand)),
        RExpr::Compare {
            op,
            lhs,
            rhs,
            collation,
        } => RExpr::Compare {
            op: *op,
            lhs: boxed(lhs),
            rhs: boxed(rhs),
            collation: collation.clone(),
        },
        RExpr::Distinct { lhs, rhs, negated } => RExpr::Distinct {
            lhs: boxed(lhs),
            rhs: boxed(rhs),
            negated: *negated,
        },
        RExpr::IsNull { operand, negated } => RExpr::IsNull {
            operand: boxed(operand),
            negated: *negated,
        },
        RExpr::ConstInt(v) => RExpr::ConstInt(*v),
        RExpr::ConstBool(v) => RExpr::ConstBool(*v),
        RExpr::ConstText(v) => RExpr::ConstText(v.clone()),
        RExpr::ConstDecimal(v) => RExpr::ConstDecimal(v.clone()),
        RExpr::ConstFloat32(v) => RExpr::ConstFloat32(*v),
        RExpr::ConstFloat64(v) => RExpr::ConstFloat64(*v),
        RExpr::ConstBytea(v) => RExpr::ConstBytea(v.clone()),
        RExpr::ConstUuid(v) => RExpr::ConstUuid(*v),
        RExpr::ConstJsonPath(v) => RExpr::ConstJsonPath(v.clone()),
        RExpr::ConstJson(v) => RExpr::ConstJson(v.clone()),
        RExpr::ConstJsonb(v) => RExpr::ConstJsonb(v.clone()),
        RExpr::ConstTimestamp(v) => RExpr::ConstTimestamp(*v),
        RExpr::ConstTimestamptz(v) => RExpr::ConstTimestamptz(*v),
        RExpr::ConstDate(v) => RExpr::ConstDate(*v),
        RExpr::ConstInterval(v) => RExpr::ConstInterval(*v),
        RExpr::ConstArray(v) => RExpr::ConstArray(v.clone()),
        RExpr::ConstRange(v) => RExpr::ConstRange(v.clone()),
        RExpr::ConstNull => RExpr::ConstNull,
        _ => unreachable!("only pushdown-safe nodes are rebased"),
    }
}
