// Logical rewrite rules — Stage 2 of the planner (spec/design/planner.md §3). The stage runs after
// resolve has built the logical plan and before optimizeSelect chooses access paths. Its rules are
// pure plan→plan transforms: they never execute anything and never read a parameter value. Mirrors
// impl/go rewrite.go. (The executor.ts ↔ rewrite.ts function cycle follows the optimize.ts
// precedent.)
//
// The predicate rewrite owns three decisions, recorded in plan.pushdown and never by mutating
// plan.filter or a join's ON (the complete predicates stay the input every access-path detector and
// estimator rule reads):
//
//   - contradiction: the WHERE's top-level AND-chain is provably never TRUE from plan-time
//     literals alone, so no relation is read and the FROM produces no rows (planner.md §3.1);
//   - WHERE pushdown: each structurally safe single-relation conjunct of a relation no outer join
//     NULL-extends moves into a derived table's body, or runs as that relation's rows are read, and
//     only the remaining conjuncts are re-applied to the joined rows (planner.md §3.2);
//   - ON pushdown: the same for a join's ON conjunct over the join's NULL-extended side, or either
//     side of an INNER join (planner.md §3.3).
//
// A body pushdown is the one decision that changes a predicate: planSelect plans the derived body
// again with the moved conjuncts appended to its WHERE (bodyPushes).

import type { JoinKind } from "./ast.ts";
import type { PlanRel, RExpr, SelectPlan } from "./executor.ts";
import {
  estimatorComparisonParts,
  estimatorComparisonSatisfied,
  estimatorConjunctionContradictory,
  estimatorFlattenBoolean,
  estimatorLiteral,
  estimatorLiteralCmp,
} from "./executor.ts";

// WherePushdown is the stage-2 predicate split. relFilters / relLocal hold one entry per relation
// (null: nothing scan-pushed); residual is the post-join WHERE when whereSplit (null when every
// conjunct moved); onResidual[k] is join k's remaining ON when onSplit[k].
export type WherePushdown = {
  contradiction: boolean;
  // relFilters are the scan-pushed filters in the plan's global slot numbering (EXPLAIN, estimator).
  relFilters: (RExpr | null)[];
  // relLocal are the same filters rebased to the relation's own row (execution).
  relLocal: (RExpr | null)[];
  // whereSplit reports that at least one WHERE conjunct moved, so residual replaces filter.
  whereSplit: boolean;
  residual: RExpr | null;
  // owners records, per top-level WHERE conjunct in source order, whether it moved (>= 0: the
  // relation it moved to; -1: residual). The uncorrelated-subquery fold replaces a folded conjunct
  // node rather than overwriting it in place (Go/Rust do), so refreshPushdownResiduals rebuilds the
  // residuals from the folded predicates.
  owners: number[];
  // onSplit[k] reports that join k's ON lost at least one conjunct; onResidual[k] then replaces it
  // at execution (null: the join has no ON predicate left). onMoved[k] is the per-conjunct moved
  // flag of join k's flattened ON (the fold refresh, as owners).
  onSplit: boolean[];
  onResidual: (RExpr | null)[];
  onMoved: boolean[][];
  // bodyPushes[i] is the conjuncts moved into derived relation i's body, rewritten to the body's
  // column slots, in source order. planSelect consumes it by planning the body again (null after).
  bodyPushes: RExpr[][] | null;
  // access is the stage-3 access predicate when an ON conjunct was scan-pushed: the complete WHERE
  // AND each scan-pushed ON conjunct in source order (null: the access predicate is the WHERE).
  // accessOn lists those scan-pushed ON conjuncts (the fold refresh rebuilds access from them).
  access: RExpr | null;
  accessOn: RExpr[];
};

// postJoinFilter is the WHERE that remains to be evaluated over the joined rows: the residual after
// a WHERE pushdown, otherwise the complete WHERE. A contradiction never reaches it (no row is
// produced).
export function postJoinFilter(sp: SelectPlan): RExpr | null {
  const pd = sp.pushdown;
  if (pd !== null && !pd.contradiction && pd.whereSplit) return pd.residual;
  return sp.filter;
}

// joinOn is the ON that join k evaluates over its candidate pairs: the residual after an ON
// pushdown (null when nothing remains), otherwise the complete ON (planner.md §3.3).
export function joinOn(sp: SelectPlan, k: number): RExpr | null {
  const pd = sp.pushdown;
  if (pd !== null && !pd.contradiction && pd.onSplit[k] === true) return pd.onResidual[k] ?? null;
  return sp.joins[k]!.on;
}

// accessPredicate is the predicate stage 3 reads for single-relation access paths: the complete
// WHERE, AND every ON conjunct scan-pushed to a relation — for its relation such a conjunct is a
// WHERE conjunct, so it may bound that relation's scan (planner.md §3.3).
export function accessPredicate(sp: SelectPlan): RExpr | null {
  const pd = sp.pushdown;
  if (pd !== null && !pd.contradiction && pd.access !== null) return pd.access;
  return sp.filter;
}

// whereContradiction reports that stage 2 proved the WHERE never TRUE.
export function whereContradiction(sp: SelectPlan): boolean {
  return sp.pushdown?.contradiction === true;
}

// pushedFilter returns relation ri's scan-pushed filter, global (glob) and rebased (local), or nulls.
export function pushedFilter(
  sp: SelectPlan,
  ri: number,
): { glob: RExpr | null; local: RExpr | null } {
  const pd = sp.pushdown;
  if (pd === null || pd.contradiction || pd.relFilters.length === 0) {
    return { glob: null, local: null };
  }
  return { glob: pd.relFilters[ri] ?? null, local: pd.relLocal[ri] ?? null };
}

// PushedConjunct is one conjunct moved by stage 2, in source order: to relation ri's body (body,
// already rewritten to body slots) or to its scan.
type PushedConjunct = { expr: RExpr; ri: number; body: boolean; on: boolean };

function contradictionPushdown(): WherePushdown {
  return {
    contradiction: true,
    relFilters: [],
    relLocal: [],
    whereSplit: false,
    residual: null,
    owners: [],
    onSplit: [],
    onResidual: [],
    onMoved: [],
    bodyPushes: null,
    access: null,
    accessOn: [],
  };
}

// rewriteWhere is the stage-2 driver: contradiction detection first (it subsumes pushdown), then
// ON pushdown (join order) and WHERE pushdown, whose moved conjuncts are collected in that source
// order. A plan without a WHERE or an ON is untouched.
export function rewriteWhere(plan: SelectPlan): void {
  const conjuncts: RExpr[] = [];
  if (plan.filter !== null) {
    estimatorFlattenBoolean(plan.filter, "and", conjuncts);
    if (whereConjunctsContradict(conjuncts)) {
      plan.pushdown = contradictionPushdown();
      return;
    }
  }
  const bodies = plan.rels.map(bodyPushable);
  // target decides the form a pushdown-safe single-relation conjunct takes (planner.md §3.2):
  // into a body-pushable derived body when every referenced output is a bare body column, else
  // a scan pushdown when the SELECT has another relation to join, else nothing.
  const target = (c: RExpr, ri: number): PushedConjunct | null => {
    if (plan.rels[ri]!.lateral === true || !pushdownSafe(c)) return null;
    const body = bodies[ri];
    if (body !== null && body !== undefined) {
      const moved = substituteBodyColumns(c, plan.rels[ri]!.offset, body);
      if (moved !== null) return { expr: moved, ri, body: true, on: false };
    }
    if (plan.rels.length < 2) return null;
    return { expr: c, ri, body: false, on: false };
  };

  const pushes: PushedConjunct[] = [];
  const onSplit = plan.joins.map(() => false);
  const onResidual: (RExpr | null)[] = plan.joins.map(() => null);
  const onMoved: boolean[][] = plan.joins.map(() => []);
  // prefixNullable[i] marks a relation NULL-extended by an earlier join (joins[0..k-1]).
  const prefixNullable = plan.rels.map(() => false);
  plan.joins.forEach((j, k) => {
    if (j.on !== null) {
      const onConjuncts: RExpr[] = [];
      estimatorFlattenBoolean(j.on, "and", onConjuncts);
      let residual: RExpr | null = null;
      for (const c of onConjuncts) {
        const ri = conjunctSingleRelation(plan, c);
        if (ri >= 0 && onPushdownSide(j.kind, k, ri, prefixNullable)) {
          const p = target(c, ri);
          if (p !== null) {
            p.on = true;
            pushes.push(p);
            onSplit[k] = true;
            onMoved[k]!.push(true);
            continue;
          }
        }
        onMoved[k]!.push(false);
        residual = andConjunct(residual, c);
      }
      onResidual[k] = residual;
    }
    markJoinNullable(prefixNullable, j.kind, k);
  });

  let whereSplit = false;
  let residual: RExpr | null = null;
  const owners: number[] = [];
  const nullable = prefixNullable; // after the loop: every relation an outer join NULL-extends
  for (const c of conjuncts) {
    const ri = conjunctSingleRelation(plan, c);
    if (ri >= 0 && !nullable[ri]) {
      const p = target(c, ri);
      if (p !== null) {
        pushes.push(p);
        whereSplit = true;
        owners.push(ri);
        continue;
      }
    }
    owners.push(-1);
    residual = andConjunct(residual, c);
  }
  if (pushes.length === 0) return;
  const pd: WherePushdown = {
    contradiction: false,
    relFilters: plan.rels.map(() => null),
    relLocal: plan.rels.map(() => null),
    whereSplit,
    residual,
    owners,
    onSplit,
    onResidual,
    onMoved,
    bodyPushes: plan.rels.map(() => []),
    access: null,
    accessOn: [],
  };
  let access = plan.filter;
  for (const p of pushes) {
    if (p.body) {
      pd.bodyPushes![p.ri]!.push(p.expr);
      continue;
    }
    pd.relFilters[p.ri] = andConjunct(pd.relFilters[p.ri]!, p.expr);
    if (p.on) {
      access = andConjunct(access, p.expr);
      pd.access = access;
      pd.accessOn.push(p.expr);
    }
  }
  pd.relFilters.forEach((f, ri) => {
    if (f !== null) pd.relLocal[ri] = rebaseColumns(f, plan.rels[ri]!.offset);
  });
  plan.pushdown = pd;
}

// refreshPushdownResiduals rebuilds a pushdown's residual WHERE, residual ONs, and access predicate
// from the (folded) predicates after the uncorrelated-subquery fold has replaced plan.filter's and
// the joins' ON conjunct nodes. The fold never changes the top-level AND shape (a folded subquery
// becomes one constant/IN node), and a moved conjunct holds no subquery, so the pushed filters and
// body pushes are unaffected and each split by moved flag is unchanged.
export function refreshPushdownResiduals(plan: SelectPlan): void {
  const pd = plan.pushdown;
  if (pd === null || pd.contradiction) return;
  plan.joins.forEach((j, k) => {
    if (!pd.onSplit[k] || j.on === null) return;
    const conjuncts: RExpr[] = [];
    estimatorFlattenBoolean(j.on, "and", conjuncts);
    const moved = pd.onMoved[k]!;
    if (conjuncts.length !== moved.length) return;
    let residual: RExpr | null = null;
    conjuncts.forEach((c, i) => {
      if (!moved[i]) residual = andConjunct(residual, c);
    });
    pd.onResidual[k] = residual;
  });
  if (plan.filter !== null) {
    const conjuncts: RExpr[] = [];
    estimatorFlattenBoolean(plan.filter, "and", conjuncts);
    if (pd.whereSplit && conjuncts.length === pd.owners.length) {
      let residual: RExpr | null = null;
      conjuncts.forEach((c, i) => {
        if (pd.owners[i]! < 0) residual = andConjunct(residual, c);
      });
      pd.residual = residual;
    }
  }
  if (pd.access !== null) {
    let access = plan.filter;
    for (const c of pd.accessOn) access = andConjunct(access, c);
    pd.access = access;
  }
}

// onPushdownSide reports whether join k's kind lets an ON conjunct over relation ri move to that
// relation (planner.md §3.3): either input of INNER, the NULL-extended input of LEFT/RIGHT, never
// FULL. A left input must not already be NULL-extended by an earlier join.
function onPushdownSide(kind: JoinKind, k: number, ri: number, prefixNullable: boolean[]): boolean {
  const right = ri === k + 1;
  switch (kind) {
    case "inner":
      return right || (ri <= k && !prefixNullable[ri]);
    case "left":
      return right;
    case "right":
      return ri <= k && !prefixNullable[ri];
    default:
      return false;
  }
}

// markJoinNullable records the relations join k NULL-extends in the left-deep FROM: joins[k]
// attaches rels[k+1] to the accumulated rels[0..k].
function markJoinNullable(nullable: boolean[], kind: JoinKind, k: number): void {
  if (kind === "left" || kind === "full") nullable[k + 1] = true;
  if (kind === "right" || kind === "full") {
    for (let i = 0; i <= k; i++) nullable[i] = true;
  }
}

// bodyPushable returns a derived relation's body when conjuncts may move into it (planner.md §3.2):
// a non-lateral single SELECT with no aggregate/GROUP BY, window function, LIMIT, or OFFSET. null
// otherwise (any other relation, or a set operation / VALUES / nested WITH body).
function bodyPushable(rel: PlanRel): SelectPlan | null {
  if (rel.derived === undefined || rel.lateral === true || rel.derived.kind !== "select")
    return null;
  const body = rel.derived;
  if (
    body.isAgg ||
    body.groupSets.length > 0 ||
    body.having !== null ||
    body.hasWindow ||
    body.limit !== null ||
    body.offset !== null
  ) {
    return null;
  }
  return body;
}

// substituteBodyColumns clones a pushdown-safe conjunct over a derived relation (whose columns start
// at offset) with every column replaced by the body column its select-list item names. null when a
// referenced output is not a bare column of the body. Only the pushdownSafe node kinds reach it;
// their leaves (literals, parameters) are shared, never mutated.
function substituteBodyColumns(e: RExpr, offset: number, body: SelectPlan): RExpr | null {
  switch (e.kind) {
    case "column": {
      const j = e.index - offset;
      if (j < 0 || j >= body.projections.length) return null;
      const item = body.projections[j]!;
      if (item.kind !== "column") return null;
      return { ...item };
    }
    case "and":
    case "or": {
      const lhs = substituteBodyColumns(e.lhs, offset, body);
      if (lhs === null) return null;
      const rhs = substituteBodyColumns(e.rhs, offset, body);
      if (rhs === null) return null;
      return { ...e, lhs, rhs };
    }
    case "not": {
      const operand = substituteBodyColumns(e.operand, offset, body);
      if (operand === null) return null;
      return { ...e, operand };
    }
    case "compare":
    case "distinct": {
      const lhs = substituteBodyColumns(e.lhs, offset, body);
      if (lhs === null) return null;
      const rhs = substituteBodyColumns(e.rhs, offset, body);
      if (rhs === null) return null;
      return { ...e, lhs, rhs };
    }
    case "isNull": {
      const operand = substituteBodyColumns(e.operand, offset, body);
      if (operand === null) return null;
      return { ...e, operand };
    }
    default:
      return e;
  }
}

// whereContradicts flattens a WHERE's top-level AND-chain and applies the plan-time contradiction
// proof. UPDATE/DELETE call it on their resolved, unfolded WHERE (planner.md §3.1), so the proof never
// depends on an uncorrelated subquery's folded value. null (no WHERE) never contradicts.
export function whereContradicts(filter: RExpr | null): boolean {
  if (filter === null) return false;
  const conjuncts: RExpr[] = [];
  estimatorFlattenBoolean(filter, "and", conjuncts);
  return whereConjunctsContradict(conjuncts);
}

// whereConjunctsContradict is the plan-time contradiction proof (planner.md §3.1): a literal FALSE
// or NULL conjunct, a comparison between two literals that is never TRUE, or the estimator's
// same-operand literal contradiction inventory (`x op NULL`, conflicting equalities, an empty range)
// restricted to comparisons whose operand is a bare column — a column has one value per row, while
// a structurally equal volatile expression (`random() > 0.9 AND random() < 0.1`) does not. A missed
// proof only forgoes the optimization; a false proof would drop rows, so every rule is sound under
// three-valued logic.
function whereConjunctsContradict(conjuncts: RExpr[]): boolean {
  const columnComparisons: RExpr[] = [];
  for (const c of conjuncts) {
    switch (c.kind) {
      case "constNull":
        return true;
      case "constBool":
        if (!c.value) return true;
        break;
      case "compare": {
        if (literalComparisonFalse(c)) return true;
        const comparison = estimatorComparisonParts(c);
        if (comparison !== null && comparison.operand.kind === "column") columnComparisons.push(c);
        break;
      }
    }
  }
  return estimatorConjunctionContradictory(columnComparisons);
}

// literalComparisonFalse proves a comparison between two plan-time literals never TRUE. Text keeps
// the estimator's byte-identity restriction to equality (ordering may use a collation).
function literalComparisonFalse(c: RExpr & { kind: "compare" }): boolean {
  const op = c.op;
  if (op !== "eq" && op !== "lt" && op !== "le" && op !== "gt" && op !== "ge") return false;
  if (!estimatorLiteral(c.lhs) || !estimatorLiteral(c.rhs)) return false;
  if (c.lhs.kind === "constNull" || c.rhs.kind === "constNull") return true;
  if ((c.lhs.kind === "constText" || c.rhs.kind === "constText") && op !== "eq") return false;
  const order = estimatorLiteralCmp(c.lhs, c.rhs);
  return order !== null && !estimatorComparisonSatisfied(order, op);
}

// conjunctSingleRelation returns the one relation whose columns c references, -1 when it references
// none or several. It walks the child shapes pushdownSafe admits (AND/OR/NOT, comparison, IS [NOT]
// NULL, IS [NOT] DISTINCT FROM); any other node is rejected by pushdownSafe, so its owner is moot.
function conjunctSingleRelation(plan: SelectPlan, c: RExpr): number {
  let owner = -1;
  let ok = true;
  const walk = (e: RExpr): void => {
    if (!ok) return;
    switch (e.kind) {
      case "column": {
        const ri = relationOfSlot(plan, e.index);
        if (ri < 0 || (owner >= 0 && owner !== ri)) {
          ok = false;
          return;
        }
        owner = ri;
        return;
      }
      case "and":
      case "or":
      case "compare":
      case "distinct":
        walk(e.lhs);
        walk(e.rhs);
        return;
      case "not":
      case "isNull":
        walk(e.operand);
        return;
      default:
        return;
    }
  };
  walk(c);
  return ok ? owner : -1;
}

function relationOfSlot(plan: SelectPlan, slot: number): number {
  for (let i = 0; i < plan.rels.length; i++) {
    const rel = plan.rels[i]!;
    if (slot >= rel.offset && slot < rel.offset + rel.colCount) return i;
  }
  return -1;
}

// pushdownSafe is the structural safety gate (planner.md §3.2): the conjunct must be unable to
// trap and must read nothing but the row, constants and parameters — so evaluating it on a row the
// join would never have produced can raise no error a non-pushed plan would not, and its value is
// the same wherever it runs. The admitted shapes are boolean combinations (AND/OR/NOT) of
// comparisons, IS [NOT] NULL, and IS [NOT] DISTINCT FROM over bare columns, literals and parameters,
// plus a bare boolean column. A collated ordering comparison is excluded (its sort-key kernel can
// fail on unloaded data); every other node kind — casts, arithmetic, functions, subqueries, outer
// references — stays in the residual.
function pushdownSafe(e: RExpr): boolean {
  switch (e.kind) {
    case "and":
    case "or":
      return pushdownSafe(e.lhs) && pushdownSafe(e.rhs);
    case "not":
      return pushdownSafe(e.operand);
    case "column":
      return true;
    case "compare":
      if (e.collation !== null && e.op !== "eq" && e.op !== "ne") return false;
      return pushdownLeaf(e.lhs) && pushdownLeaf(e.rhs);
    case "distinct":
      return pushdownLeaf(e.lhs) && pushdownLeaf(e.rhs);
    case "isNull":
      return pushdownLeaf(e.operand);
    default:
      return false;
  }
}

function pushdownLeaf(e: RExpr): boolean {
  return e.kind === "column" || e.kind === "param" || estimatorLiteral(e);
}

// andConjunct appends c to a left-deep AND chain (source order is preserved by the caller).
function andConjunct(acc: RExpr | null, c: RExpr): RExpr {
  if (acc === null) return c;
  return { kind: "and", lhs: acc, rhs: c };
}

// rebaseColumns clones a pushdown-safe tree with every column slot shifted down by offset, so it
// evaluates against the relation's own row. Only the pushdownSafe node kinds reach it; their leaves
// (literals, parameters) are shared, never mutated.
function rebaseColumns(e: RExpr, offset: number): RExpr {
  switch (e.kind) {
    case "column":
      return { ...e, index: e.index - offset };
    case "and":
    case "or":
      return { kind: e.kind, lhs: rebaseColumns(e.lhs, offset), rhs: rebaseColumns(e.rhs, offset) };
    case "not":
      return { kind: "not", operand: rebaseColumns(e.operand, offset) };
    case "compare":
      return {
        ...e,
        lhs: rebaseColumns(e.lhs, offset),
        rhs: rebaseColumns(e.rhs, offset),
      };
    case "distinct":
      return {
        ...e,
        lhs: rebaseColumns(e.lhs, offset),
        rhs: rebaseColumns(e.rhs, offset),
      };
    case "isNull":
      return { ...e, operand: rebaseColumns(e.operand, offset) };
    default:
      return e;
  }
}
