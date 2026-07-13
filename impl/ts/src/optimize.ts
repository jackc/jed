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
import type {
  Engine,
  HashJoinPlan,
  IndexOrder,
  RExpr,
  ScanBound,
  ScanCandidate,
  ScanCandidateIdentity,
  ScopeRel,
  SelectPlan,
} from "./executor.ts";
import {
  SCAN_CANDIDATE_KINDS,
  compareLowerName,
  collectTouched,
  detectINLBound,
  estimateScanCandidates,
  fkTypesEqual,
  inventoryScanCandidates,
  inventoryINLCandidates,
  needsEagerScan,
  orderSatisfiedByIndex,
  orderSatisfiedByIndexes,
  orderSatisfiedByPK,
  physicalRelOrdinal,
  relationColumnRange,
  scanBoundHasStorageOrder,
  SELECT_SCAN_BOUND_POLICY,
  selectCostedScanCandidate,
  selectLegacyScanCandidate,
} from "./executor.ts";
import { JOIN_DP_LIMIT } from "./estimator_constants.ts";
import type { Snapshot } from "./snapshot.ts";
import { isAttachmentScope } from "./session.ts";
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
  ruleCostedSingleRelationPipeline(plan, rels, snap, eng);
  ruleCostedTwoRelationJoin(plan, rels, snap, eng);
  ruleCostedNWayJoin(plan, rels, snap, eng);
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
  plan.phys.hashJoin = buildHashJoinPlan(plan, rels, 0, 1);
}

function buildHashJoinPlan(
  plan: SelectPlan,
  rels: ScopeRel[],
  outer: number,
  inner: number,
): HashJoinPlan | null {
  return buildHashJoinPlanForOns(plan, rels, [outer], inner, [0]);
}

function buildHashJoinPlanForOns(
  plan: SelectPlan,
  rels: ScopeRel[],
  outers: number[],
  inner: number,
  onIndices: number[],
): HashJoinPlan | null {
  if (onIndices.length === 0) return null;
  const conjuncts: RExpr[] = [];
  for (const onIndex of onIndices) {
    const on = plan.joins[onIndex]!.on;
    if (on === null) return null;
    flattenHashJoinConjuncts(on, conjuncts);
  }
  const innerRange = {
    start: rels[inner]!.offset,
    end: rels[inner]!.offset + rels[inner]!.table.columns.length,
  };
  const contains = (range: { start: number; end: number }, index: number): boolean =>
    index >= range.start && index < range.end;
  const isOuter = (index: number): boolean =>
    outers.some((ordinal) => {
      const rel = rels[ordinal]!;
      return index >= rel.offset && index < rel.offset + rel.table.columns.length;
    });
  const keys: { left: number; right: number; type: Type }[] = [];
  for (const expr of conjuncts) {
    if (!hashJoinSafeConjunct(expr)) return null;
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
    if (contains(innerRange, left) && isOuter(right)) [left, right] = [right, left];
    if (!isOuter(left) || !contains(innerRange, right)) continue;
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
  return keys.length > 0 ? { keys } : null;
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
    const estimates = estimateScanCandidates(
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
    plan.phys.relEstimates[i] = estimates;
    return rels.length === 1
      ? selectCostedScanCandidate(candidates, estimates, SELECT_SCAN_BOUND_POLICY)
      : selectLegacyScanCandidate(candidates, SELECT_SCAN_BOUND_POLICY);
  });
}

type SingleRelationPipelineCandidate = {
  identity: ScanCandidateIdentity;
  bound: ScanBound | null;
  pkOrdered: boolean;
  pkReverse: boolean;
  indexOrder: IndexOrder | null;
};

// P6b composes every legal access path with its natural ordering, adds missing order-only B-tree
// top-N walks, and selects the minimum cumulative scheduled estimate through LIMIT/OFFSET. A
// blocking sort adds no private weight. Canonical identity order breaks exact cost ties.
function ruleCostedSingleRelationPipeline(
  plan: SelectPlan,
  rels: ScopeRel[],
  snap: Snapshot,
  eng: Engine,
): void {
  if (
    rels.length !== 1 ||
    plan.rels.length !== 1 ||
    plan.rels[0]!.srf !== undefined ||
    plan.rels[0]!.cte !== undefined ||
    plan.rels[0]!.derived !== undefined
  ) {
    return;
  }
  const rel = rels[0]!;
  const access = inventoryScanCandidates(plan.filter, rel, snap, eng);
  if (access.length === 0) return;

  const pkDir =
    !plan.isAgg && plan.order.length > 0 && plan.orderExprs.length === 0
      ? orderSatisfiedByPK(snap, rel.table, plan.rels[0]!.offset, plan.order)
      : null;
  const indexOrders =
    !isAttachmentScope(rel.db) &&
    !plan.isAgg &&
    !plan.hasWindow &&
    !plan.distinct &&
    plan.order.length > 0 &&
    plan.orderExprs.length === 0 &&
    pkDir === null
      ? orderSatisfiedByIndexes(snap, rel.table, plan.rels[0]!.offset, plan.order)
      : [];

  const orderByName = new Map(indexOrders.map((order) => [order.nameKey, order]));
  const pipelines: SingleRelationPipelineCandidate[] = access.map((candidate) => {
    const storageOrder = candidate.scanOrder.kind === "storageKey";
    const indexOrder =
      candidate.scanOrder.kind === "indexKey"
        ? (orderByName.get(candidate.scanOrder.indexName) ?? null)
        : null;
    return {
      identity: candidate.identity,
      bound: candidate.bound,
      pkOrdered: storageOrder && pkDir !== null,
      pkReverse: storageOrder && (pkDir?.reverse ?? false),
      indexOrder,
    };
  });
  const identityKey = (identity: ScanCandidateIdentity): string =>
    `${identity.kind}\u0000${identity.indexName}`;
  const seen = new Set(pipelines.map((candidate) => identityKey(candidate.identity)));
  for (const indexOrder of indexOrders) {
    if (plan.limit === null) break; // the established order-only eligibility gate requires LIMIT
    const identity: ScanCandidateIdentity = { kind: "btree", indexName: indexOrder.nameKey };
    if (seen.has(identityKey(identity))) continue;
    pipelines.push({
      identity,
      bound: null,
      pkOrdered: false,
      pkReverse: false,
      indexOrder,
    });
  }
  pipelines.sort((a, b) => {
    const rank =
      SCAN_CANDIDATE_KINDS.indexOf(a.identity.kind) - SCAN_CANDIDATE_KINDS.indexOf(b.identity.kind);
    if (rank !== 0) return rank;
    return a.identity.indexName < b.identity.indexName
      ? -1
      : a.identity.indexName > b.identity.indexName
        ? 1
        : 0;
  });

  let winner: SingleRelationPipelineCandidate | null = null;
  let winnerCost = 0n;
  for (const candidate of pipelines) {
    const trial: SelectPlan = {
      ...plan,
      phys: {
        ...plan.phys,
        relBounds: [candidate.bound],
        pkOrdered: candidate.pkOrdered,
        pkReverse: candidate.pkReverse,
        indexOrder: candidate.indexOrder,
        joinPkOrdered: false,
        topK: null,
      },
    };
    const cost = eng.estimateSelectPlanCost(trial);
    if (winner === null || cost < winnerCost) {
      winner = candidate;
      winnerCost = cost;
    }
  }
  if (winner === null) return;
  plan.phys.relBounds[0] = winner.bound;
  plan.phys.pkOrdered = winner.pkOrdered;
  plan.phys.pkReverse = winner.pkReverse;
  plan.phys.indexOrder = winner.indexOrder;
}

type JoinAlgorithm = "inl" | "hash" | "nested";

type TwoRelationCandidate = {
  order: [number, number];
  outerIdentity: ScanCandidateIdentity;
  innerIdentity: ScanCandidateIdentity;
  algorithm: JoinAlgorithm;
  outerBound: ScanBound | null;
  innerBound: ScanBound | null;
  innerINL: ScanBound | null;
  hashJoin: HashJoinPlan | null;
};

function compareScanIdentity(a: ScanCandidateIdentity, b: ScanCandidateIdentity): number {
  const rank = SCAN_CANDIDATE_KINDS.indexOf(a.kind) - SCAN_CANDIDATE_KINDS.indexOf(b.kind);
  return rank !== 0 ? rank : compareLowerName({ name: a.indexName }, { name: b.indexName });
}

// P7 exhaustively composes every ordinary access path and legal join algorithm in both physical
// orientations. Logical expression slots remain in source order; relationOrder changes only which
// relation drives and which relation builds/seeks.
function ruleCostedTwoRelationJoin(
  plan: SelectPlan,
  rels: ScopeRel[],
  snap: Snapshot,
  eng: Engine,
): void {
  if (
    rels.length !== 2 ||
    plan.rels.length !== 2 ||
    plan.joins.length !== 1 ||
    (plan.joins[0]!.kind !== "inner" && plan.joins[0]!.kind !== "cross") ||
    plan.rels.some(
      (rel) =>
        rel.lateral || rel.srf !== undefined || rel.cte !== undefined || rel.derived !== undefined,
    )
  ) {
    return;
  }

  const ordinary = [
    inventoryScanCandidates(plan.filter, rels[0]!, snap, eng),
    inventoryScanCandidates(plan.filter, rels[1]!, snap, eng),
  ];
  const candidates: TwoRelationCandidate[] = [];
  for (const order of [
    [0, 1],
    [1, 0],
  ] as [number, number][]) {
    const [outer, inner] = order;
    const hashJoin = buildHashJoinPlan(plan, rels, outer, inner);
    for (const outerAccess of ordinary[outer]!) {
      for (const innerAccess of ordinary[inner]!) {
        if (hashJoin !== null) {
          candidates.push({
            order,
            outerIdentity: outerAccess.identity,
            innerIdentity: innerAccess.identity,
            algorithm: "hash",
            outerBound: outerAccess.bound,
            innerBound: innerAccess.bound,
            innerINL: null,
            hashJoin,
          });
        }
        candidates.push({
          order,
          outerIdentity: outerAccess.identity,
          innerIdentity: innerAccess.identity,
          algorithm: "nested",
          outerBound: outerAccess.bound,
          innerBound: innerAccess.bound,
          innerINL: null,
          hashJoin: null,
        });
      }
      for (const innerAccess of inventoryINLCandidates(
        plan.joins[0]!.on,
        plan.filter,
        rels[inner]!,
        relationColumnRange(plan, outer),
        snap,
      )) {
        candidates.push({
          order,
          outerIdentity: outerAccess.identity,
          innerIdentity: innerAccess.identity,
          algorithm: "inl",
          outerBound: outerAccess.bound,
          innerBound: null,
          innerINL: innerAccess.bound,
          hashJoin: null,
        });
      }
    }
  }
  const algorithmRank: Record<JoinAlgorithm, number> = { inl: 0, hash: 1, nested: 2 };
  candidates.sort((a, b) => {
    const order = a.order[0] - b.order[0] || a.order[1] - b.order[1];
    if (order !== 0) return order;
    const outer = compareScanIdentity(a.outerIdentity, b.outerIdentity);
    if (outer !== 0) return outer;
    const inner = compareScanIdentity(a.innerIdentity, b.innerIdentity);
    return inner !== 0 ? inner : algorithmRank[a.algorithm] - algorithmRank[b.algorithm];
  });

  let winner: TwoRelationCandidate | null = null;
  let winnerCost = 0n;
  for (const candidate of candidates) {
    const [outer, inner] = candidate.order;
    const relBounds: (ScanBound | null)[] = [null, null];
    const relINLBounds: (ScanBound | null)[] = [null, null];
    relBounds[outer] = candidate.outerBound;
    relBounds[inner] = candidate.innerBound;
    relINLBounds[inner] = candidate.innerINL;
    const trial: SelectPlan = {
      ...plan,
      phys: {
        ...plan.phys,
        relationOrder: [...candidate.order],
        relBounds,
        relINLBounds,
        hashJoin: candidate.hashJoin,
        pkOrdered: false,
        pkReverse: false,
        indexOrder: null,
        joinPkOrdered: false,
        topK: null,
      },
    };
    trial.phys.joinPkOrdered = joinPkOrderedForCandidate(trial, rels, snap);
    const cost = eng.estimateSelectPlanCost(trial);
    if (winner === null || cost < winnerCost) {
      winner = candidate;
      winnerCost = cost;
    }
  }
  if (winner === null) return;
  const [outer, inner] = winner.order;
  plan.phys.relationOrder = [...winner.order];
  plan.phys.relBounds = [null, null];
  plan.phys.relINLBounds = [null, null];
  plan.phys.relBounds[outer] = winner.outerBound;
  plan.phys.relBounds[inner] = winner.innerBound;
  plan.phys.relINLBounds[inner] = winner.innerINL;
  plan.phys.hashJoin = winner.hashJoin;
}

type JoinSearchAccess = {
  identity: ScanCandidateIdentity;
  bound: ScanBound | null;
  inl: boolean;
  // -1 means the sibling predicate came from WHERE rather than an authored ON tree.
  onIndex: number;
};

type JoinSearchStep = { algorithm: JoinAlgorithm; onIndices: number[] };

type JoinSearchState = {
  order: number[];
  access: JoinSearchAccess[];
  steps: JoinSearchStep[];
  estimate: { cost: bigint; rows: bigint; logicalRows: bigint };
  satisfiesQueryOrder: boolean;
};

function cloneJoinSearchState(state: JoinSearchState): JoinSearchState {
  return {
    ...state,
    order: [...state.order],
    access: state.access.map((access) => ({ ...access })),
    steps: state.steps.map((step) => ({ ...step, onIndices: [...step.onIndices] })),
    estimate: { ...state.estimate },
  };
}

function compareNumberArrays(a: number[], b: number[]): number {
  for (let i = 0; i < a.length && i < b.length; i++) {
    if (a[i] !== b[i]) return a[i]! - b[i]!;
  }
  return a.length - b.length;
}

function compareJoinSearchState(a: JoinSearchState, b: JoinSearchState): number {
  let comparison = compareNumberArrays(a.order, b.order);
  if (comparison !== 0) return comparison;
  for (let i = 0; i < a.access.length; i++) {
    comparison = compareScanIdentity(a.access[i]!.identity, b.access[i]!.identity);
    if (comparison !== 0) return comparison;
    if (a.access[i]!.inl && b.access[i]!.inl && a.access[i]!.onIndex !== b.access[i]!.onIndex) {
      return a.access[i]!.onIndex - b.access[i]!.onIndex;
    }
  }
  const algorithmRank: Record<JoinAlgorithm, number> = { inl: 0, hash: 1, nested: 2 };
  for (let i = 0; i < a.steps.length; i++) {
    comparison = algorithmRank[a.steps[i]!.algorithm] - algorithmRank[b.steps[i]!.algorithm];
    if (comparison !== 0) return comparison;
    comparison = compareNumberArrays(a.steps[i]!.onIndices, b.steps[i]!.onIndices);
    if (comparison !== 0) return comparison;
  }
  return 0;
}

function joinSearchMask(state: JoinSearchState): number {
  let mask = 0;
  for (const ordinal of state.order) mask |= 1 << ordinal;
  return mask;
}

function joinFrontierIndex(mask: number, ordered: boolean): number {
  return mask * 2 + (ordered ? 1 : 0);
}

function insertJoinFrontier(frontier: JoinSearchState[], candidate: JoinSearchState): void {
  const { cost, rows, logicalRows } = candidate.estimate;
  for (const prior of frontier) {
    const weak =
      prior.estimate.cost <= cost &&
      prior.estimate.rows <= rows &&
      prior.estimate.logicalRows <= logicalRows;
    const strict =
      prior.estimate.cost < cost ||
      prior.estimate.rows < rows ||
      prior.estimate.logicalRows < logicalRows;
    const same =
      prior.estimate.cost === cost &&
      prior.estimate.rows === rows &&
      prior.estimate.logicalRows === logicalRows;
    if ((weak && strict) || (same && compareJoinSearchState(prior, candidate) <= 0)) return;
  }
  for (let i = frontier.length - 1; i >= 0; i--) {
    const prior = frontier[i]!;
    const weak =
      cost <= prior.estimate.cost &&
      rows <= prior.estimate.rows &&
      logicalRows <= prior.estimate.logicalRows;
    const strict =
      cost < prior.estimate.cost ||
      rows < prior.estimate.rows ||
      logicalRows < prior.estimate.logicalRows;
    if (weak && strict) frontier.splice(i, 1);
  }
  frontier.push(candidate);
  frontier.sort(compareJoinSearchState);
}

function newlyReadyOnIndices(plan: SelectPlan, before: boolean[], after: boolean[]): number[] {
  const ready: number[] = [];
  const totalColumns = plan.rels.reduce((total, rel) => total + rel.colCount, 0);
  for (let index = 0; index < plan.joins.length; index++) {
    const on = plan.joins[index]!.on;
    if (on === null) continue;
    const touched = new Array<boolean>(totalColumns).fill(false);
    collectTouched(on, 0, touched);
    const dependencies = plan.rels.map((rel) =>
      touched.slice(rel.offset, rel.offset + rel.colCount).some(Boolean),
    );
    dependencies[index + 1] = true;
    const readyBefore = dependencies.every((needed, ordinal) => !needed || before[ordinal]!);
    const readyAfter = dependencies.every((needed, ordinal) => !needed || after[ordinal]!);
    if (readyAfter && !readyBefore) ready.push(index);
  }
  return ready;
}

function installJoinSearchState(plan: SelectPlan, rels: ScopeRel[], state: JoinSearchState): void {
  const n = plan.rels.length;
  plan.phys.relationOrder = [...state.order];
  const present = new Set(state.order);
  for (let ordinal = 0; ordinal < n; ordinal++) {
    if (!present.has(ordinal)) plan.phys.relationOrder.push(ordinal);
  }
  plan.phys.relBounds = new Array<ScanBound | null>(n).fill(null);
  plan.phys.relINLBounds = new Array<ScanBound | null>(n).fill(null);
  plan.phys.hashJoin = null;
  for (let position = 0; position < state.access.length; position++) {
    const ordinal = state.order[position]!;
    const access = state.access[position]!;
    if (access.inl) plan.phys.relINLBounds[ordinal] = access.bound;
    else plan.phys.relBounds[ordinal] = access.bound;
  }
  plan.phys.joinSteps = state.steps.map((step, position) => {
    const inner = state.order[position + 1]!;
    const hashJoin =
      step.algorithm === "hash"
        ? buildHashJoinPlanForOns(
            plan,
            rels,
            state.order.slice(0, position + 1),
            inner,
            step.onIndices,
          )
        : null;
    return { onIndices: [...step.onIndices], hashJoin };
  });
  plan.phys.pkOrdered = false;
  plan.phys.pkReverse = false;
  plan.phys.indexOrder = null;
  plan.phys.joinPkOrdered = false;
  plan.phys.topK = null;
}

function refreshJoinSearchState(
  plan: SelectPlan,
  rels: ScopeRel[],
  state: JoinSearchState,
  eng: Engine,
): void {
  installJoinSearchState(plan, rels, state);
  state.estimate = eng.estimateJoinSearchPrefix(plan, state.order.length);
}

function nwayDriverSatisfiesOrder(
  plan: SelectPlan,
  rels: ScopeRel[],
  driver: number,
  snap: Snapshot,
): boolean {
  if (
    plan.isAgg ||
    plan.hasWindow ||
    plan.distinct ||
    plan.order.length === 0 ||
    plan.orderExprs.length !== 0 ||
    plan.limit === null
  ) {
    return false;
  }
  const bound = plan.phys.relBounds[driver];
  if (bound !== null && bound?.kind !== "pk") return false;
  const direction = orderSatisfiedByPK(
    snap,
    rels[driver]!.table,
    plan.rels[driver]!.offset,
    plan.order,
  );
  return direction !== null && !direction.reverse;
}

function expandJoinSearchState(
  plan: SelectPlan,
  rels: ScopeRel[],
  state: JoinSearchState,
  snap: Snapshot,
  eng: Engine,
): JoinSearchState[] {
  const n = plan.rels.length;
  const present = new Array<boolean>(n).fill(false);
  const siblingColumns = state.order.flatMap((ordinal) => relationColumnRange(plan, ordinal));
  for (const ordinal of state.order) present[ordinal] = true;
  const out: JoinSearchState[] = [];
  for (let inner = 0; inner < n; inner++) {
    if (present[inner]) continue;
    const after = [...present];
    after[inner] = true;
    const onIndices = newlyReadyOnIndices(plan, present, after);
    const inl: { candidate: ScanCandidate; onIndex: number }[] = [];
    for (const onIndex of onIndices) {
      for (const candidate of inventoryINLCandidates(
        plan.joins[onIndex]!.on,
        null,
        rels[inner]!,
        siblingColumns,
        snap,
      )) {
        inl.push({ candidate, onIndex });
      }
    }
    for (const candidate of inventoryINLCandidates(
      null,
      plan.filter,
      rels[inner]!,
      siblingColumns,
      snap,
    )) {
      inl.push({ candidate, onIndex: -1 });
    }
    inl.sort((a, b) => {
      const access = compareScanIdentity(a.candidate.identity, b.candidate.identity);
      return access !== 0 ? access : a.onIndex - b.onIndex;
    });
    for (const choice of inl) {
      const candidate = cloneJoinSearchState(state);
      candidate.order.push(inner);
      candidate.access.push({
        identity: choice.candidate.identity,
        bound: choice.candidate.bound,
        inl: true,
        onIndex: choice.onIndex,
      });
      candidate.steps.push({ algorithm: "inl", onIndices: [...onIndices] });
      refreshJoinSearchState(plan, rels, candidate, eng);
      out.push(candidate);
    }
    const hasHash = buildHashJoinPlanForOns(plan, rels, state.order, inner, onIndices) !== null;
    for (const access of inventoryScanCandidates(plan.filter, rels[inner]!, snap, eng)) {
      if (hasHash) {
        const candidate = cloneJoinSearchState(state);
        candidate.order.push(inner);
        candidate.access.push({
          identity: access.identity,
          bound: access.bound,
          inl: false,
          onIndex: -1,
        });
        candidate.steps.push({ algorithm: "hash", onIndices: [...onIndices] });
        refreshJoinSearchState(plan, rels, candidate, eng);
        out.push(candidate);
      }
      const candidate = cloneJoinSearchState(state);
      candidate.order.push(inner);
      candidate.access.push({
        identity: access.identity,
        bound: access.bound,
        inl: false,
        onIndex: -1,
      });
      candidate.steps.push({ algorithm: "nested", onIndices: [...onIndices] });
      refreshJoinSearchState(plan, rels, candidate, eng);
      out.push(candidate);
    }
  }
  out.sort(compareJoinSearchState);
  return out;
}

function popcount(value: number): number {
  let count = 0;
  while (value !== 0) {
    value &= value - 1;
    count++;
  }
  return count;
}

// P8's bounded deterministic N-way search. Up to JOIN_DP_LIMIT relations use subset DP with a
// Pareto frontier over cumulative cost, physical rows, and logical rows per requested-order
// property. Larger islands use the same canonical candidate expansion greedily.
function ruleCostedNWayJoin(plan: SelectPlan, rels: ScopeRel[], snap: Snapshot, eng: Engine): void {
  const n = plan.rels.length;
  if (
    n < 3 ||
    rels.length !== n ||
    plan.joins.length + 1 !== n ||
    plan.joins.some((join) => join.kind !== "inner" && join.kind !== "cross") ||
    plan.rels.some(
      (rel) =>
        rel.lateral || rel.srf !== undefined || rel.cte !== undefined || rel.derived !== undefined,
    )
  ) {
    return;
  }

  const initialState = (ordinal: number, access: ScanCandidate): JoinSearchState => {
    const state: JoinSearchState = {
      order: [ordinal],
      access: [{ identity: access.identity, bound: access.bound, inl: false, onIndex: -1 }],
      steps: [],
      estimate: { cost: 0n, rows: 0n, logicalRows: 0n },
      satisfiesQueryOrder: false,
    };
    refreshJoinSearchState(plan, rels, state, eng);
    state.satisfiesQueryOrder = nwayDriverSatisfiesOrder(plan, rels, ordinal, snap);
    return state;
  };

  let winner: JoinSearchState | null = null;
  if (n <= JOIN_DP_LIMIT) {
    const frontiers: JoinSearchState[][] = Array.from({ length: (1 << n) * 2 }, () => []);
    for (let ordinal = 0; ordinal < n; ordinal++) {
      for (const access of inventoryScanCandidates(plan.filter, rels[ordinal]!, snap, eng)) {
        const state = initialState(ordinal, access);
        insertJoinFrontier(
          frontiers[joinFrontierIndex(joinSearchMask(state), state.satisfiesQueryOrder)]!,
          state,
        );
      }
    }
    for (let size = 1; size < n; size++) {
      for (let mask = 1; mask < 1 << n; mask++) {
        if (popcount(mask) !== size) continue;
        for (const ordered of [false, true]) {
          const states = [...frontiers[joinFrontierIndex(mask, ordered)]!];
          for (const state of states) {
            for (const candidate of expandJoinSearchState(plan, rels, state, snap, eng)) {
              insertJoinFrontier(
                frontiers[
                  joinFrontierIndex(joinSearchMask(candidate), candidate.satisfiesQueryOrder)
                ]!,
                candidate,
              );
            }
          }
        }
      }
    }
    const full = (1 << n) - 1;
    const completed = [
      ...frontiers[joinFrontierIndex(full, false)]!,
      ...frontiers[joinFrontierIndex(full, true)]!,
    ].sort(compareJoinSearchState);
    let winnerCost = 0n;
    for (const state of completed) {
      installJoinSearchState(plan, rels, state);
      plan.phys.joinPkOrdered =
        state.satisfiesQueryOrder && joinPkOrderedForCandidate(plan, rels, snap);
      const cost = eng.estimateSelectPlanCost(plan);
      if (winner === null || cost < winnerCost) {
        winner = state;
        winnerCost = cost;
      }
    }
  } else {
    const drivers: JoinSearchState[] = [];
    for (let ordinal = 0; ordinal < n; ordinal++) {
      for (const access of inventoryScanCandidates(plan.filter, rels[ordinal]!, snap, eng)) {
        drivers.push(initialState(ordinal, access));
      }
    }
    drivers.sort((a, b) =>
      a.estimate.cost === b.estimate.cost
        ? compareJoinSearchState(a, b)
        : a.estimate.cost < b.estimate.cost
          ? -1
          : 1,
    );
    winner = drivers[0] ?? null;
    while (winner !== null && winner.order.length < n) {
      const next = expandJoinSearchState(plan, rels, winner, snap, eng).sort((a, b) =>
        a.estimate.cost === b.estimate.cost
          ? compareJoinSearchState(a, b)
          : a.estimate.cost < b.estimate.cost
            ? -1
            : 1,
      );
      winner = next[0] ?? null;
    }
  }
  if (winner === null) return;
  installJoinSearchState(plan, rels, winner);
  plan.phys.joinPkOrdered =
    winner.satisfiesQueryOrder && joinPkOrderedForCandidate(plan, rels, snap);
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
    plan.rels[0]!.derived === undefined &&
    scanBoundHasStorageOrder(plan.phys.relBounds[0])
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
  plan.phys.joinPkOrdered = joinPkOrderedForCandidate(plan, rels, snap);
}

function joinPkOrderedForCandidate(plan: SelectPlan, rels: ScopeRel[], snap: Snapshot): boolean {
  if (
    plan.rels.length < 2 ||
    rels.length !== plan.rels.length ||
    plan.joins.length + 1 !== plan.rels.length
  ) {
    return false;
  }
  const outer = physicalRelOrdinal(plan, 0);
  const inner = physicalRelOrdinal(plan, plan.rels.length - 1);
  if (
    !plan.isAgg &&
    !plan.hasWindow &&
    !plan.distinct &&
    plan.order.length > 0 &&
    plan.orderExprs.length === 0 &&
    plan.limit !== null &&
    plan.joins.every((join) => join.kind === "inner" || join.kind === "cross") &&
    plan.rels.every(
      (r) =>
        r.lateral !== true && r.srf === undefined && r.cte === undefined && r.derived === undefined,
    ) &&
    !needsEagerScan(plan.phys.relBounds[outer]) &&
    plan.phys.relINLBounds[outer] === null &&
    (plan.phys.relINLBounds[inner] === null ||
      plan.phys.relINLBounds[inner]!.kind === "pk" ||
      plan.phys.relINLBounds[inner]!.kind === "index" ||
      plan.phys.relINLBounds[inner]!.kind === "gin" ||
      plan.phys.relINLBounds[inner]!.kind === "gist") &&
    plan.order.length <= pkIndices(rels[outer]!.table).length
  ) {
    const dir = orderSatisfiedByPK(snap, rels[outer]!.table, plan.rels[outer]!.offset, plan.order);
    return dir !== null && !dir.reverse;
  }
  return false;
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
