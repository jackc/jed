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
import type { Engine, ScopeRel, SelectPlan } from "./executor.ts";
import {
  detectINLBound,
  detectScanBound,
  needsEagerScan,
  orderSatisfiedByIndex,
  orderSatisfiedByPK,
} from "./executor.ts";
import type { Snapshot } from "./snapshot.ts";

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
  ruleOrderByPkScan(plan, rels, snap);
  ruleOrderByIndexScan(plan, rels, snap);
  ruleJoinPkOrdered(plan, rels, snap);
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
  const filter = plan.filter;
  plan.phys.relBounds = rels.map((rel, i) =>
    filter === null ||
    plan.rels[i]!.srf !== undefined ||
    plan.rels[i]!.cte !== undefined ||
    plan.rels[i]!.derived !== undefined
      ? null
      : detectScanBound(filter, rel, snap, eng),
  );
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
// INNER/CROSS join (cost.md §3 "JOIN"): the nested loop drives the outer (rels[0]) in PK order, so
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
