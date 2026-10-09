// Logical rewrite rules — Stage 2 of the planner (spec/design/planner.md §3). The stage runs after
// resolve has built the logical plan and before optimizeSelect chooses access paths. Its rules are
// pure plan→plan transforms: they never execute anything and never read a parameter value. Mirrors
// impl/go rewrite.go. (The executor.ts ↔ rewrite.ts function cycle follows the optimize.ts
// precedent.)
//
// The WHERE rewrite owns two decisions, recorded in plan.pushdown and never by mutating plan.filter
// (the complete WHERE stays the input every access-path detector and estimator rule reads):
//
//   - contradiction: the WHERE's top-level AND-chain is provably never TRUE from plan-time
//     literals alone, so no relation is read and the FROM produces no rows (planner.md §3.1);
//   - pushdown: in a multi-relation SELECT, each structurally safe single-base-relation conjunct of
//     a relation no outer join NULL-extends is evaluated as that relation's rows are read, and only
//     the remaining conjuncts are re-applied to the joined rows (planner.md §3.2).

import type { PlanRel, RExpr, SelectPlan } from "./executor.ts";
import {
  estimatorComparisonParts,
  estimatorComparisonSatisfied,
  estimatorConjunctionContradictory,
  estimatorFlattenBoolean,
  estimatorLiteral,
  estimatorLiteralCmp,
} from "./executor.ts";

// WherePushdown is the stage-2 WHERE split. relFilters / relLocal hold one entry per relation (null:
// nothing pushed); residual is the post-join WHERE (null when every conjunct was pushed).
export type WherePushdown = {
  contradiction: boolean;
  // relFilters are the pushed filters in the plan's global slot numbering (EXPLAIN, estimator).
  relFilters: (RExpr | null)[];
  // relLocal are the same filters rebased to the relation's own row (execution).
  relLocal: (RExpr | null)[];
  residual: RExpr | null;
  // owners records, per top-level WHERE conjunct in source order, the relation it was pushed to
  // (-1: residual). The uncorrelated-subquery fold replaces a folded conjunct node rather than
  // overwriting it in place, so refreshWhereResidual rebuilds the residual from the folded WHERE.
  owners: number[];
};

// postJoinFilter is the WHERE that remains to be evaluated over the joined rows: the residual after
// a pushdown, otherwise the complete WHERE. A contradiction never reaches it (no row is produced).
export function postJoinFilter(sp: SelectPlan): RExpr | null {
  if (sp.pushdown !== null && !sp.pushdown.contradiction) return sp.pushdown.residual;
  return sp.filter;
}

// whereContradiction reports that stage 2 proved the WHERE never TRUE.
export function whereContradiction(sp: SelectPlan): boolean {
  return sp.pushdown?.contradiction === true;
}

// pushedFilter returns relation ri's pushed filter, global (glob) and rebased (local), or nulls.
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

// rewriteWhere is the stage-2 driver: contradiction detection first (it subsumes pushdown), then
// WHERE pushdown. A plan without a WHERE is untouched.
export function rewriteWhere(plan: SelectPlan): void {
  if (plan.filter === null) return;
  const conjuncts: RExpr[] = [];
  estimatorFlattenBoolean(plan.filter, "and", conjuncts);
  if (whereConjunctsContradict(conjuncts)) {
    plan.pushdown = {
      contradiction: true,
      relFilters: [],
      relLocal: [],
      residual: null,
      owners: [],
    };
    return;
  }
  if (plan.rels.length < 2) return;
  const nullable = nullableRelations(plan);
  const owners: number[] = [];
  let pushed = false;
  for (const c of conjuncts) {
    owners.push(-1);
    const ri = conjunctSingleRelation(plan, c);
    if (ri < 0 || nullable[ri] || !pushdownTarget(plan.rels[ri]!) || !pushdownSafe(c)) continue;
    owners[owners.length - 1] = ri;
    pushed = true;
  }
  if (!pushed) return;
  const pd: WherePushdown = {
    contradiction: false,
    relFilters: plan.rels.map(() => null),
    relLocal: plan.rels.map(() => null),
    residual: null,
    owners,
  };
  conjuncts.forEach((c, i) => {
    const ri = owners[i]!;
    if (ri >= 0) pd.relFilters[ri] = andConjunct(pd.relFilters[ri]!, c);
    else pd.residual = andConjunct(pd.residual, c);
  });
  pd.relFilters.forEach((f, ri) => {
    if (f !== null) pd.relLocal[ri] = rebaseColumns(f, plan.rels[ri]!.offset);
  });
  plan.pushdown = pd;
}

// refreshWhereResidual rebuilds a pushdown's residual from the (folded) WHERE after the
// uncorrelated-subquery fold has replaced plan.filter's conjunct nodes. The fold never changes the
// top-level AND shape (a folded subquery becomes one constant/IN node), and a pushed conjunct holds
// no subquery, so the pushed filters are unaffected and the split by owner is unchanged.
export function refreshWhereResidual(plan: SelectPlan): void {
  const pd = plan.pushdown;
  if (pd === null || pd.contradiction || plan.filter === null) return;
  const conjuncts: RExpr[] = [];
  estimatorFlattenBoolean(plan.filter, "and", conjuncts);
  if (conjuncts.length !== pd.owners.length) return;
  let residual: RExpr | null = null;
  conjuncts.forEach((c, i) => {
    if (pd.owners[i]! < 0) residual = andConjunct(residual, c);
  });
  pd.residual = residual;
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

// nullableRelations marks every relation an outer join NULL-extends in the left-deep FROM:
// joins[k] attaches rels[k+1] to the accumulated rels[0..k].
function nullableRelations(plan: SelectPlan): boolean[] {
  const nullable = plan.rels.map(() => false);
  plan.joins.forEach((j, k) => {
    if (j.kind === "left" || j.kind === "full") nullable[k + 1] = true;
    if (j.kind === "right" || j.kind === "full") {
      for (let i = 0; i <= k; i++) nullable[i] = true;
    }
  });
  return nullable;
}

// pushdownTarget admits only a non-lateral base table: its rows are read by an access path, so the
// pushed filter runs exactly where the scan admits each row.
function pushdownTarget(rel: PlanRel): boolean {
  return (
    rel.srf === undefined &&
    rel.cte === undefined &&
    rel.derived === undefined &&
    rel.lateral !== true
  );
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
