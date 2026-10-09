//! Logical rewrite rules — Stage 2 of the planner (spec/design/planner.md §3). The stage runs after
//! resolve has built the logical plan and before `optimize_select` chooses access paths. Its rules
//! are pure plan→plan transforms: they never execute anything and never read a parameter value.
//!
//! The predicate rewrite owns three decisions, recorded in `plan.pushdown` and never by mutating
//! the WHERE or a join's ON (the complete predicates stay the input every access-path detector and
//! estimator rule reads):
//!
//!   - contradiction: the WHERE's top-level AND-chain is provably never TRUE from plan-time
//!     literals alone, so no relation is read and the FROM produces no rows (planner.md §3.1);
//!   - WHERE pushdown: each structurally safe single-relation conjunct of a relation no outer join
//!     NULL-extends moves into a derived table's body, or runs as that relation's rows are read, and
//!     only the remaining conjuncts are re-applied to the joined rows (planner.md §3.2);
//!   - ON pushdown: the same for a join's ON conjunct over the join's NULL-extended side, or either
//!     side of an INNER join (planner.md §3.3).
//!
//! A body pushdown is the one decision that changes a predicate: `plan_select` plans the derived
//! body again with the moved conjuncts appended to its WHERE — to each SELECT of a set operation,
//! and below a grouped SELECT's grouping (`body_pushes`).
//!
//! Rust note: a resolved `RExpr` is not `Clone` (a subquery owns its plan), so a residual cannot be
//! an owned tree beside the WHERE / ON. It is recorded as the residual conjuncts' positions in the
//! flattened predicate and borrowed as a [`FilterChain`] — the same nodes the Go core's residual
//! shares with its `filter` / ON, so the uncorrelated-subquery fold reaches both. The pushed filters
//! hold only pushdown-safe nodes, so they are real (cloned) trees. The stage-3 access predicate (the
//! WHERE AND each scan-pushed ON conjunct) must be a real tree that contains the WHERE, so
//! [`PlanFilter`] owns it and reaches the WHERE down its left spine.

use super::*;

/// A SELECT's WHERE, owned together with the stage-3 **access predicate** built around it. When no
/// ON conjunct is scan-pushed (the common case) the tree IS the WHERE. Otherwise stage 2 wraps it as
/// the left-deep `((WHERE AND c1) AND c2)` of the scan-pushed ON conjuncts in source order (or
/// `(c1 AND c2)` with no WHERE), so the WHERE is the node `appended` steps down the left spine.
pub(crate) struct PlanFilter {
    tree: Option<RExpr>,
    appended: usize,
    has_where: bool,
}

impl PlanFilter {
    pub(crate) fn new(filter: Option<RExpr>) -> Self {
        PlanFilter {
            has_where: filter.is_some(),
            tree: filter,
            appended: 0,
        }
    }

    /// The complete WHERE (`None`: the SELECT has none).
    pub(crate) fn get(&self) -> Option<&RExpr> {
        if !self.has_where {
            return None;
        }
        let mut e = self.tree.as_ref()?;
        for _ in 0..self.appended {
            let RExpr::And(lhs, _) = e else {
                unreachable!("the access predicate's left spine holds the WHERE")
            };
            e = lhs;
        }
        Some(e)
    }

    /// The complete WHERE, mutably (the post-bind uncorrelated-subquery fold).
    pub(crate) fn get_mut(&mut self) -> Option<&mut RExpr> {
        if !self.has_where {
            return None;
        }
        let mut e = self.tree.as_mut()?;
        for _ in 0..self.appended {
            let RExpr::And(lhs, _) = e else {
                unreachable!("the access predicate's left spine holds the WHERE")
            };
            e = lhs;
        }
        Some(e)
    }

    /// The complete WHERE, by value (tests that inspect a planned WHERE).
    #[cfg(test)]
    pub(crate) fn into_where(self) -> Option<RExpr> {
        if !self.has_where {
            return None;
        }
        let mut e = self.tree?;
        for _ in 0..self.appended {
            let RExpr::And(lhs, _) = e else {
                unreachable!("the access predicate's left spine holds the WHERE")
            };
            e = *lhs;
        }
        Some(e)
    }

    pub(crate) fn is_some(&self) -> bool {
        self.has_where
    }

    /// The predicate stage 3 reads for single-relation access paths: the complete WHERE AND every
    /// ON conjunct scan-pushed to a relation — for its relation such a conjunct is a WHERE
    /// conjunct, so it may bound that relation's scan (planner.md §3.3).
    pub(crate) fn access(&self) -> Option<&RExpr> {
        self.tree.as_ref()
    }

    fn append_access(&mut self, c: RExpr) {
        self.tree = Some(and_conjunct(self.tree.take(), c));
        self.appended += 1;
    }
}

/// The stage-2 predicate split. `rel_filters` / `rel_local` hold one entry per relation (`None`:
/// nothing scan-pushed; all empty on a contradiction). `residual` lists the positions, in the
/// flattened top-level AND-chain of the WHERE, of the conjuncts left for the joined rows when
/// `where_split` (empty when every conjunct moved); `on_residual[k]` does the same for join `k`'s ON
/// when `on_split[k]`.
pub(crate) struct WherePushdown {
    contradiction: bool,
    /// The scan-pushed filters in the plan's global slot numbering (EXPLAIN, estimator).
    rel_filters: Vec<Option<RExpr>>,
    /// The same filters rebased to the relation's own row (execution).
    rel_local: Vec<Option<RExpr>>,
    /// At least one WHERE conjunct moved, so `residual` replaces the WHERE over joined rows.
    where_split: bool,
    residual: Vec<usize>,
    /// `on_split[k]`: join `k`'s ON lost at least one conjunct; `on_residual[k]` then replaces it at
    /// execution (empty: the join has no ON predicate left).
    on_split: Vec<bool>,
    on_residual: Vec<Vec<usize>>,
    /// `body_pushes[i]`: the conjuncts moved into derived relation `i`'s body, rebased to the body's
    /// output columns, in source order. `plan_select` consumes them by planning the body again.
    body_pushes: Vec<Vec<RExpr>>,
}

/// A post-join predicate: the left-deep AND, in source order, of `conjuncts`. A one-element chain is
/// that expression itself — the complete WHERE / ON when nothing was pushed. Every operation below
/// reproduces exactly what the same operation does on the equivalent `RExpr::And` tree (eval order,
/// guards and charges, rendering, estimator recursion), so the chain is observably that tree.
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

/// The chain of the conjuncts at `positions` in `predicate`'s flattened top-level AND-chain (`None`
/// when no position is left).
fn residual_chain<'a>(predicate: &'a RExpr, positions: &[usize]) -> Option<FilterChain<'a>> {
    if positions.is_empty() {
        return None;
    }
    let mut all = Vec::new();
    estimator_flatten_boolean(predicate, true, &mut all);
    Some(FilterChain {
        conjuncts: positions.iter().map(|&i| all[i]).collect(),
    })
}

impl SelectPlan {
    /// The WHERE that remains to be evaluated over the joined rows: the residual after a WHERE
    /// pushdown, otherwise the complete WHERE. A contradiction never reaches it (no row is produced).
    pub(crate) fn post_join_filter(&self) -> Option<FilterChain<'_>> {
        let filter = self.filter.get()?;
        match &self.pushdown {
            Some(pd) if !pd.contradiction && pd.where_split => residual_chain(filter, &pd.residual),
            _ => Some(FilterChain {
                conjuncts: vec![filter],
            }),
        }
    }

    /// Whether [`SelectPlan::post_join_filter`] is present (without building the chain).
    pub(crate) fn has_post_join_filter(&self) -> bool {
        match &self.pushdown {
            Some(pd) if !pd.contradiction && pd.where_split => !pd.residual.is_empty(),
            _ => self.filter.is_some(),
        }
    }

    /// The ON that join `k` evaluates over its candidate pairs: the residual after an ON pushdown
    /// (`None` when nothing remains), otherwise the complete ON (planner.md §3.3).
    pub(crate) fn join_on(&self, k: usize) -> Option<FilterChain<'_>> {
        let on = self.joins[k].on.as_ref()?;
        match &self.pushdown {
            Some(pd) if !pd.contradiction && pd.on_split.get(k).copied().unwrap_or(false) => {
                residual_chain(on, &pd.on_residual[k])
            }
            _ => Some(FilterChain {
                conjuncts: vec![on],
            }),
        }
    }

    /// [`SelectPlan::join_on`] of every join, in join order — built once before a per-pair loop.
    pub(crate) fn join_ons(&self) -> Vec<Option<FilterChain<'_>>> {
        (0..self.joins.len()).map(|k| self.join_on(k)).collect()
    }

    /// Stage 2 proved the WHERE never TRUE (planner.md §3.1).
    pub(crate) fn where_contradiction(&self) -> bool {
        self.pushdown.as_ref().is_some_and(|pd| pd.contradiction)
    }

    /// Relation `ri`'s scan-pushed filter in global slots (EXPLAIN, estimator), or `None`.
    pub(crate) fn pushed_filter(&self, ri: usize) -> Option<&RExpr> {
        let pd = self.pushdown.as_ref()?;
        if pd.contradiction {
            return None;
        }
        pd.rel_filters.get(ri)?.as_ref()
    }

    /// Relation `ri`'s scan-pushed filter rebased to the relation's own row (execution), or `None`.
    pub(crate) fn pushed_local_filter(&self, ri: usize) -> Option<&RExpr> {
        let pd = self.pushdown.as_ref()?;
        if pd.contradiction {
            return None;
        }
        pd.rel_local.get(ri)?.as_ref()
    }

    /// Take the conjuncts stage 2 moved into each derived body (one list per relation, empty when
    /// none), leaving none behind: `plan_select` plans those bodies again (planner.md §3.2).
    pub(crate) fn take_body_pushes(&mut self) -> Vec<Vec<RExpr>> {
        match &mut self.pushdown {
            Some(pd) if !pd.contradiction => std::mem::take(&mut pd.body_pushes),
            _ => Vec::new(),
        }
    }
}

/// One conjunct moved by stage 2, in source order: to relation `ri`'s body (`body`, already
/// rewritten to body slots) or to its scan; `on` marks a conjunct from a join's ON.
struct PushedConjunct {
    expr: RExpr,
    ri: usize,
    body: bool,
    on: bool,
}

/// The stage-2 driver: contradiction detection first (it subsumes pushdown), then ON pushdown (join
/// order) and WHERE pushdown, whose moved conjuncts are collected in that source order. A plan
/// without a WHERE or an ON is untouched.
pub(crate) fn rewrite_where(plan: &mut SelectPlan) {
    let mut conjuncts = Vec::new();
    if let Some(filter) = plan.filter.get() {
        estimator_flatten_boolean(filter, true, &mut conjuncts);
        if where_conjuncts_contradict(&conjuncts) {
            plan.pushdown = Some(WherePushdown {
                contradiction: true,
                rel_filters: Vec::new(),
                rel_local: Vec::new(),
                where_split: false,
                residual: Vec::new(),
                on_split: Vec::new(),
                on_residual: Vec::new(),
                body_pushes: Vec::new(),
            });
            return;
        }
    }
    let bodies: Vec<Option<&QueryPlan>> = plan.rels.iter().map(body_pushable).collect();
    // The form a pushdown-safe single-relation conjunct takes (planner.md §3.2): into a
    // body-pushable derived body when every referenced output is a bare body column, else a scan
    // pushdown when the SELECT has another relation to join, else nothing.
    let target = |c: &RExpr, ri: usize| -> Option<(RExpr, bool)> {
        if plan.rels[ri].lateral || !pushdown_safe(c) {
            return None;
        }
        if let Some(body) = bodies[ri]
            && let Some(moved) = body_conjunct(c, plan.rels[ri].offset, body)
        {
            return Some((moved, true));
        }
        if plan.rels.len() < 2 {
            return None;
        }
        Some((clone_pushdown_safe(c), false))
    };

    let mut pushes: Vec<PushedConjunct> = Vec::new();
    let mut on_split = vec![false; plan.joins.len()];
    let mut on_residual: Vec<Vec<usize>> = vec![Vec::new(); plan.joins.len()];
    // prefix_nullable[i] marks a relation NULL-extended by an earlier join (joins[0..k-1]).
    let mut prefix_nullable = vec![false; plan.rels.len()];
    for (k, j) in plan.joins.iter().enumerate() {
        if let Some(on) = &j.on {
            let mut on_conjuncts = Vec::new();
            estimator_flatten_boolean(on, true, &mut on_conjuncts);
            for (pos, c) in on_conjuncts.iter().enumerate() {
                if let Some(ri) = conjunct_single_relation(plan, c)
                    && on_pushdown_side(j.kind, k, ri, &prefix_nullable)
                    && let Some((expr, body)) = target(c, ri)
                {
                    pushes.push(PushedConjunct {
                        expr,
                        ri,
                        body,
                        on: true,
                    });
                    on_split[k] = true;
                    continue;
                }
                on_residual[k].push(pos);
            }
        }
        mark_join_nullable(&mut prefix_nullable, j.kind, k);
    }

    let mut where_split = false;
    let mut residual = Vec::new();
    // After the loop: every relation an outer join NULL-extends.
    let nullable = prefix_nullable;
    for (pos, c) in conjuncts.iter().enumerate() {
        if let Some(ri) = conjunct_single_relation(plan, c)
            && !nullable[ri]
            && let Some((expr, body)) = target(c, ri)
        {
            pushes.push(PushedConjunct {
                expr,
                ri,
                body,
                on: false,
            });
            where_split = true;
            continue;
        }
        residual.push(pos);
    }
    drop(conjuncts);
    if pushes.is_empty() {
        return;
    }
    let n = plan.rels.len();
    let mut rel_filters: Vec<Option<RExpr>> = (0..n).map(|_| None).collect();
    let mut body_pushes: Vec<Vec<RExpr>> = (0..n).map(|_| Vec::new()).collect();
    for p in pushes {
        if p.body {
            body_pushes[p.ri].push(p.expr);
            continue;
        }
        if p.on {
            plan.filter.append_access(clone_pushdown_safe(&p.expr));
        }
        let acc = rel_filters[p.ri].take();
        rel_filters[p.ri] = Some(and_conjunct(acc, p.expr));
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
        where_split,
        residual,
        on_split,
        on_residual,
        body_pushes,
    });
}

/// Whether join `k`'s kind lets an ON conjunct over relation `ri` move to that relation (planner.md
/// §3.3): either input of INNER, the NULL-extended input of LEFT/RIGHT, never FULL. A left input
/// must not already be NULL-extended by an earlier join.
fn on_pushdown_side(kind: JoinKind, k: usize, ri: usize, prefix_nullable: &[bool]) -> bool {
    let right = ri == k + 1;
    match kind {
        JoinKind::Inner => right || (ri <= k && !prefix_nullable[ri]),
        JoinKind::Left => right,
        JoinKind::Right => ri <= k && !prefix_nullable[ri],
        _ => false,
    }
}

/// Record the relations join `k` NULL-extends in the left-deep FROM: `joins[k]` attaches
/// `rels[k+1]` to the accumulated `rels[0..=k]`.
fn mark_join_nullable(nullable: &mut [bool], kind: JoinKind, k: usize) {
    if matches!(kind, JoinKind::Left | JoinKind::Full) {
        nullable[k + 1] = true;
    }
    if matches!(kind, JoinKind::Right | JoinKind::Full) {
        for n in nullable.iter_mut().take(k + 1) {
            *n = true;
        }
    }
}

/// A derived relation's body when conjuncts may move into it (planner.md §3.2): a non-lateral body
/// that is a body-pushable SELECT or a set operation of them. `None` otherwise (any other relation,
/// or a VALUES / nested WITH body).
fn body_pushable(rel: &PlanRel) -> Option<&QueryPlan> {
    if rel.lateral {
        return None;
    }
    let body = rel.derived.as_deref()?;
    query_body_pushable(body).then_some(body)
}

/// The structural half of body-pushability (planner.md §3.2): a SELECT with no window function,
/// LIMIT, or OFFSET that is either ungrouped or grouped by exactly one grouping set with at least one
/// key; or a set operation without LIMIT/OFFSET whose two arms are body-pushable.
fn query_body_pushable(q: &QueryPlan) -> bool {
    match q {
        QueryPlan::Select(b) => {
            if b.has_window || b.limit.is_some() || b.offset.is_some() {
                return false;
            }
            !b.is_agg || (b.group_sets.len() == 1 && !b.group_keys.is_empty())
        }
        QueryPlan::SetOp(so) => {
            so.limit.is_none()
                && so.offset.is_none()
                && query_body_pushable(&so.lhs)
                && query_body_pushable(&so.rhs)
        }
        _ => false,
    }
}

/// The FROM slot a body-pushable SELECT's output column `j` reads; `None` when the select-list item
/// is not a bare column. In a grouped body the item must be a grouping column: a plain input column
/// of the master grouping list (an expression key's synthetic slot is not).
fn body_input_column(b: &SelectPlan, j: usize) -> Option<usize> {
    let RExpr::Column(idx) = b.projections.get(j)? else {
        return None;
    };
    if !b.is_agg {
        return Some(*idx);
    }
    let gk = *b.group_keys.get(*idx)?;
    let width: usize = b.rels.iter().map(|r| r.col_count).sum();
    (gk < width).then_some(gk)
}

/// Whether output column `j` of a body-pushable plan reads a bare body column of type `want` in
/// every SELECT it combines: a set operation's arms (and its own unified column) must already have
/// that exact type, so no arm value is widened after the pushed conjunct would have judged it.
fn body_output_pushable(q: &QueryPlan, j: usize, want: &ResolvedType) -> bool {
    match q {
        QueryPlan::Select(b) => body_input_column(b, j).is_some() && b.column_types[j] == *want,
        QueryPlan::SetOp(so) => {
            so.column_types[j] == *want
                && body_output_pushable(&so.lhs, j, want)
                && body_output_pushable(&so.rhs, j, want)
        }
        _ => false,
    }
}

/// Clone a pushdown-safe conjunct over a derived relation (whose columns start at `offset`) rebased
/// to the body's output-column numbering; `None` when a referenced output column is not
/// body-pushable ([`body_output_pushable`]). `replan_pushed_bodies` later substitutes each output
/// column with the body column it reads, separately in each set-operation arm.
fn body_conjunct(e: &RExpr, offset: usize, body: &QueryPlan) -> Option<RExpr> {
    let types = body.column_types();
    map_pushdown_safe(e, &|slot| {
        let j = slot.checked_sub(offset)?;
        (j < types.len() && body_output_pushable(body, j, &types[j])).then_some(j)
    })
}

/// Clone a conjunct in a SELECT body's output-column numbering with every column replaced by the
/// FROM slot that output reads. [`body_conjunct`] already proved each reference maps.
pub(crate) fn substitute_body_columns(e: &RExpr, body: &SelectPlan) -> RExpr {
    map_pushdown_safe(e, &|j| body_input_column(body, j))
        .expect("body_conjunct proved every output column maps")
}

/// Flatten a WHERE's top-level AND-chain and apply the plan-time contradiction proof. UPDATE/DELETE
/// call it on their resolved, unfolded WHERE (planner.md §3.1), so the proof never depends on an
/// uncorrelated subquery's folded value. `None` (no WHERE) never contradicts.
pub(crate) fn where_contradicts(filter: Option<&RExpr>) -> bool {
    let Some(filter) = filter else {
        return false;
    };
    let mut conjuncts = Vec::new();
    estimator_flatten_boolean(filter, true, &mut conjuncts);
    where_conjuncts_contradict(&conjuncts)
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
    map_pushdown_safe(e, &|slot| Some(slot - offset)).expect("every column slot is rebased")
}

/// Clone a pushdown-safe tree with every column slot mapped through `column` (`None` from it fails
/// the clone). Only the `pushdown_safe` node kinds and the `estimator_literal` leaves reach it.
fn map_pushdown_safe(e: &RExpr, column: &dyn Fn(usize) -> Option<usize>) -> Option<RExpr> {
    let boxed = |e: &RExpr| map_pushdown_safe(e, column).map(Box::new);
    Some(match e {
        RExpr::Column(i) => RExpr::Column(column(*i)?),
        RExpr::Param(i) => RExpr::Param(*i),
        RExpr::And(l, r) => RExpr::And(boxed(l)?, boxed(r)?),
        RExpr::Or(l, r) => RExpr::Or(boxed(l)?, boxed(r)?),
        RExpr::Not(operand) => RExpr::Not(boxed(operand)?),
        RExpr::Compare {
            op,
            lhs,
            rhs,
            collation,
        } => RExpr::Compare {
            op: *op,
            lhs: boxed(lhs)?,
            rhs: boxed(rhs)?,
            collation: collation.clone(),
        },
        RExpr::Distinct { lhs, rhs, negated } => RExpr::Distinct {
            lhs: boxed(lhs)?,
            rhs: boxed(rhs)?,
            negated: *negated,
        },
        RExpr::IsNull { operand, negated } => RExpr::IsNull {
            operand: boxed(operand)?,
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
        _ => unreachable!("only pushdown-safe nodes are cloned"),
    })
}
