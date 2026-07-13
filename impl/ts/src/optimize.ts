// Physical / access-path selection — Stage 3 of the planner (spec/design/planner.md §4). The
// optimizeSelect pass runs after the resolve half has built the logical plan (planSelect,
// executor.ts) and applies each optimization as a DISCRETE RULE: one function owning its gate (the
// structural pattern it requires) and its action (the plan.phys fields it sets). A rule that does
// not fire leaves its fields zero-valued — the executor then takes the always-correct unoptimized
// path (full scan, eager sort). The pattern-matching MECHANISMS the rules call (detectScanBound,
// detectINLBound, orderSatisfiedByPK, orderSatisfiedByIndex) live in executor.ts — they also serve
// UPDATE/DELETE planning and exec-time eligibility, so they are machinery, not rules. Mirrors
// impl/go optimize.go / impl/rust executor/optimize.rs. (The executor.ts ↔ optimize.ts function
// cycle follows the session.ts precedent.)

import { pkIndices } from "./catalog.ts";
import type { Engine, RExpr, ScopeRel, SelectPlan } from "./executor.ts";
import {
  detectINLBound,
  estimateScanCandidates,
  fkTypesEqual,
  inventoryScanCandidates,
  needsEagerScan,
  orderSatisfiedByIndex,
  orderSatisfiedByPK,
  SELECT_SCAN_BOUND_POLICY,
  selectLegacyScanCandidate,
} from "./executor.ts";
import type { Snapshot } from "./snapshot.ts";
import type { ScalarType, Type } from "./types.ts";

// optimizeSelect applies the physical rules to a freshly resolved logical plan, in a FIXED order
// that is part of the cross-core contract (spec/design/planner.md §4): later rules read earlier
// rules' output — ruleOrderByIndexScan reads relBounds[0] (ruleScanBounds) and pkOrdered
// (ruleOrderByPkScan); ruleJoinPkOrdered reads relBounds[0] and relINLBounds. rels is the resolve
// scope's relation list — the rules need the Table references the owned plan deliberately drops
// (PlanRel carries only names, so the plan outlives the scope).
export function optimizeSelect(
  plan: SelectPlan,
  rels: ScopeRel[],
  snap: Snapshot,
  eng: Engine,
): void {
  ruleScanBounds(plan, rels, snap, eng);
  ruleIndexNestedLoop(plan, rels, snap);
  ruleHashJoin(plan, rels);
  ruleOrderByPkScan(plan, rels, snap);
  ruleOrderByIndexScan(plan, rels, snap);
  ruleJoinPkOrdered(plan, rels, snap);
  ruleOrderByLimitTopK(plan);
}

// ruleHashJoin selects the deterministic two-input in-memory hash operator after INL has had first
// refusal. ON must be an AND-chain of non-trapping leaf equality/inequality comparisons, with at
// least one same-type bare-column equality crossing the inputs. Crossing equalities become keys in
// source order; the full ON remains authoritative at execution.
function ruleHashJoin(plan: SelectPlan, rels: ScopeRel[]): void {
  if (
    rels.length !== 2 ||
    plan.joins.length !== 1 ||
    plan.rels[0]!.lateral ||
    plan.rels[1]!.lateral ||
    plan.phys.relINLBounds[1] !== null ||
    (plan.joins[0]!.kind !== "inner" && plan.joins[0]!.kind !== "left") ||
    plan.joins[0]!.on === null
  ) {
    return;
  }
  const conjuncts: RExpr[] = [];
  flattenHashJoinConjuncts(plan.joins[0]!.on!, conjuncts);
  const rightOffset = rels[1]!.offset;
  const keys: { left: number; right: number; type: Type }[] = [];
  for (const expr of conjuncts) {
    if (!hashJoinSafeConjunct(expr)) return;
    if (
      expr.kind !== "compare" ||
      expr.op !== "eq" ||
      expr.lhs.kind !== "column" ||
      expr.rhs.kind !== "column"
    ) {
      continue;
    }
    let left = expr.lhs.index;
    let right = expr.rhs.index;
    if (left >= rightOffset && right < rightOffset) [left, right] = [right, left];
    if (left >= rightOffset || right < rightOffset) continue;
    const leftType = hashJoinColumnType(rels, left);
    const rightType = hashJoinColumnType(rels, right);
    if (
      leftType === null ||
      rightType === null ||
      !fkTypesEqual(leftType, rightType) ||
      !hashJoinKeyableType(leftType)
    ) {
      continue;
    }
    keys.push({ left, right, type: leftType });
  }
  if (keys.length > 0) plan.phys.hashJoin = { keys };
}

function flattenHashJoinConjuncts(expr: RExpr, out: RExpr[]): void {
  if (expr.kind === "and") {
    flattenHashJoinConjuncts(expr.lhs, out);
    flattenHashJoinConjuncts(expr.rhs, out);
  } else {
    out.push(expr);
  }
}

function hashJoinSafeConjunct(expr: RExpr): boolean {
  return (
    expr.kind === "compare" &&
    (expr.op === "eq" || expr.op === "ne") &&
    hashJoinLeaf(expr.lhs) &&
    hashJoinLeaf(expr.rhs)
  );
}

function hashJoinLeaf(expr: RExpr): boolean {
  return expr.kind === "column" || expr.kind.startsWith("const");
}

function hashJoinColumnType(rels: ScopeRel[], index: number): Type | null {
  for (const rel of rels) {
    const local = index - rel.offset;
    if (local >= 0 && local < rel.table.columns.length) return rel.table.columns[local]!.type;
  }
  return null;
}

function hashJoinKeyableType(type: Type): boolean {
  if (type.kind === "composite") return false;
  if (type.kind === "array" || type.kind === "range") {
    return type.elem.kind === "scalar" && hashJoinKeyableScalar(type.elem.scalar);
  }
  return hashJoinKeyableScalar(type.scalar);
}

function hashJoinKeyableScalar(type: ScalarType): boolean {
  return type !== "json" && type !== "jsonb" && type !== "jsonpath";
}

// ruleScanBounds — primary-key / index predicate pushdown, per base relation: detect WHERE
// conjuncts that bound that relation's storage key, so its scan seeks/ranges instead of walking
// the whole B-tree (cost.md §3 "bounded scan"). The filter is resolved against the full FROM
// scope, so a relation's PK column is the GLOBAL index rel.offset+pkLocal; isConstSource only
// accepts a literal/param/outer const (never a sibling column), so a JOIN base table is bounded
// only by a CONSTANT predicate on its own PK — `b.pk = a.x` is ruleIndexNestedLoop's case. Sound
// for outer joins too: a non-NULL PK conjunct in WHERE eliminates that relation's NULL-extended
// rows, so bounding it cannot drop a surviving row. A no-PK relation gets null (full scan).
// A set-returning relation is a computed row source with no PK/index — it never bounds
// (functions.md §10), so skip detection for it. A CTE reference is likewise a computed/buffered
// source with no store PK (cte.md §5), so skip it too.
function ruleScanBounds(plan: SelectPlan, rels: ScopeRel[], snap: Snapshot, eng: Engine): void {
  plan.phys.relEstimates = rels.map(() => []);
  plan.phys.relBounds = rels.map((rel, i) => {
    if (
      plan.rels[i]!.srf !== undefined ||
      plan.rels[i]!.cte !== undefined ||
      plan.rels[i]!.derived !== undefined
    ) {
      return null;
    }
    const candidates = inventoryScanCandidates(plan.filter, rel, snap, eng);
    plan.phys.relEstimates[i] = estimateScanCandidates(
      candidates,
      rel,
      eng,
      rels.length === 1 &&
        !plan.isAgg &&
        !plan.distinct &&
        plan.limit === null &&
        plan.offset === null &&
        !plan.hasWindow,
    );
    return selectLegacyScanCandidate(candidates, SELECT_SCAN_BOUND_POLICY);
  });
}

// ruleIndexNestedLoop — index-nested-loop pushdown (cost.md §3 "JOIN"): a join inner relation
// whose primary key / indexed column is compared to a SIBLING column of an earlier relation
// (`a JOIN b ON b.pk = a.x`) is re-materialized per outer row, seeking instead of full-scanning —
// O(N·M) → O(N·log M). Detected from the join's ON and the WHERE. Gated to a base table (an SRF /
// derived table / CTE / lateral item has no store to seek) that is the RIGHT/nullable side of an
// INNER/CROSS/LEFT join (a RIGHT/FULL preserved side cannot be bounded per outer row). rels[0] has
// no earlier relation; relation i's join is plan.joins[i-1]. A non-null entry takes precedence
// over the once-materialized relBounds.
function ruleIndexNestedLoop(plan: SelectPlan, rels: ScopeRel[], snap: Snapshot): void {
  plan.phys.relINLBounds = rels.map((rel, i) => {
    if (
      i === 0 ||
      plan.rels[i]!.srf !== undefined ||
      plan.rels[i]!.derived !== undefined ||
      plan.rels[i]!.cte !== undefined ||
      plan.rels[i]!.lateral
    ) {
      return null;
    }
    const k = plan.joins[i - 1]!.kind;
    if (k !== "inner" && k !== "cross" && k !== "left") return null;
    return detectINLBound(plan.joins[i - 1]!.on, plan.filter, rel, snap);
  });
}

// ruleOrderByPkScan — ORDER BY satisfied by primary-key scan order (spec/design/cost.md §3): a
// single base table, non-aggregate SELECT whose ORDER BY keys are a prefix of the relation's
// PRIMARY KEY columns — collation-matching the column's stored key form, all in one direction
// (ASC ⇒ forward scan, DESC ⇒ a reverse scan over the full PK) — needs no sort, since the table
// scan already yields rows in that order. The streaming scan then elides the sort (and, with a
// LIMIT, short-circuits a top-N). DISTINCT is allowed: the dedup runs streaming in scan order,
// keeping the first occurrence, and the sort is elided (cost.md §3 "DISTINCT").
function ruleOrderByPkScan(plan: SelectPlan, rels: ScopeRel[], snap: Snapshot): void {
  const pkDir =
    !plan.isAgg &&
    plan.order.length > 0 &&
    plan.orderExprs.length === 0 &&
    plan.rels.length === 1 &&
    plan.rels[0]!.srf === undefined &&
    plan.rels[0]!.cte === undefined &&
    plan.rels[0]!.derived === undefined
      ? orderSatisfiedByPK(snap, rels[0]!.table, plan.rels[0]!.offset, plan.order)
      : null;
  plan.phys.pkOrdered = pkDir !== null;
  plan.phys.pkReverse = pkDir?.reverse ?? false;
}

// ruleOrderByIndexScan — ORDER BY satisfied by SECONDARY-INDEX scan order (cost.md §3): when the
// PK scan does NOT satisfy the order but a B-tree index's columns do, and there is a LIMIT, walk
// that index and point-look-up each row — a top-N that avoids the blocking sort. Gated to a LIMIT
// and, when a WHERE bound exists, only when that bound walks the same index in the same order;
// mutually exclusive with pkOrdered.
function ruleOrderByIndexScan(plan: SelectPlan, rels: ScopeRel[], snap: Snapshot): void {
  const candidate =
    !plan.isAgg &&
    !plan.hasWindow &&
    !plan.distinct &&
    !plan.phys.pkOrdered &&
    plan.limit !== null &&
    plan.order.length > 0 &&
    plan.orderExprs.length === 0 &&
    plan.rels.length === 1 &&
    plan.rels[0]!.srf === undefined &&
    plan.rels[0]!.cte === undefined &&
    plan.rels[0]!.derived === undefined
      ? orderSatisfiedByIndex(snap, rels[0]!.table, plan.rels[0]!.offset, plan.order)
      : null;
  const bound = plan.phys.relBounds[0]!;
  plan.phys.indexOrder =
    candidate !== null &&
    (bound === null ||
      (bound.kind === "index" && bound.index.nameKey === candidate.nameKey) ||
      (bound.kind === "indexSet" && bound.indexSet.nameKey === candidate.nameKey))
      ? candidate
      : null;
}

// ruleJoinPkOrdered — ORDER BY satisfied by the OUTER relation's PK scan order in a two-table
// INNER/CROSS join (cost.md §3 "JOIN"): the join drives/probes the outer (rels[0]) in PK order, so
// the join output is already in (outer PK, inner key) order — the sort is elided, and with a LIMIT
// the loop short-circuits a top-N. Gated to exactly two non-lateral base relations, an INNER/CROSS
// join, a LIMIT, and a FORWARD outer-PK order with NO key beyond the outer PK (an extra key is a
// real tie-break the outer scan order does not satisfy — the outer PK is not unique over the join
// output). The outer must carry no non-PK bound (a PK bound / no bound keeps it in PK order); the
// optional inner INL must be PK/B-tree so its per-outer materialization preserves eager key order.
function ruleJoinPkOrdered(plan: SelectPlan, rels: ScopeRel[], snap: Snapshot): void {
  if (
    !plan.isAgg &&
    !plan.hasWindow &&
    !plan.distinct &&
    plan.order.length > 0 &&
    plan.orderExprs.length === 0 &&
    plan.limit !== null &&
    plan.rels.length === 2 &&
    plan.joins.length === 1 &&
    (plan.joins[0]!.kind === "inner" || plan.joins[0]!.kind === "cross") &&
    plan.rels.every(
      (r) =>
        r.lateral !== true && r.srf === undefined && r.cte === undefined && r.derived === undefined,
    ) &&
    !needsEagerScan(plan.phys.relBounds[0]) &&
    plan.phys.relINLBounds[0] === null &&
    (plan.phys.relINLBounds[1] === null ||
      plan.phys.relINLBounds[1]!.kind === "pk" ||
      plan.phys.relINLBounds[1]!.kind === "index" ||
      plan.phys.relINLBounds[1]!.kind === "gin" ||
      plan.phys.relINLBounds[1]!.kind === "gist") &&
    plan.order.length <= pkIndices(rels[0]!.table).length
  ) {
    const dir = orderSatisfiedByPK(snap, rels[0]!.table, plan.rels[0]!.offset, plan.order);
    plan.phys.joinPkOrdered = dir !== null && !dir.reverse;
  }
}

// ruleOrderByLimitTopK — bounded selection for a BLOCKING ORDER BY with a constant LIMIT. Plain
// SELECT pre-sort rows have one deterministic sequence across base scans, joins, SRFs, CTEs, and
// derived relations. DISTINCT, aggregate/group, and window plans stay excluded. Earlier sort
// elision rules win. LIMIT 0 records K=0 regardless of OFFSET; bigint addition cannot overflow.
function ruleOrderByLimitTopK(plan: SelectPlan): void {
  if (
    plan.isAgg ||
    plan.hasWindow ||
    plan.distinct ||
    plan.order.length === 0 ||
    plan.limit === null ||
    plan.phys.pkOrdered ||
    plan.phys.indexOrder !== null ||
    plan.phys.joinPkOrdered
  ) {
    return;
  }
  if (plan.limit === 0n) {
    plan.phys.topK = 0n;
    return;
  }
  const k = (plan.offset ?? 0n) + plan.limit;
  if (k <= 9223372036854775807n) plan.phys.topK = k;
}
