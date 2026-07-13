// Hand-written deterministic Path-B estimator arithmetic. Shared constants live in the generated
// estimator_constants.ts; selectivity folds and runtime-unit accounting remain native per core.

import {
  type EstimatorFraction,
  ESTIMATOR_ACCESS_PATH_ORDER,
  ESTIMATOR_UNIT_COUNT,
  ESTIMATOR_UNIT_WEIGHTS,
  MAX_ESTIMATE,
  UNIT_GIN_ENTRY,
  UNIT_GIST_DESCENT,
  UNIT_OPERATOR_EVAL,
  UNIT_PAGE_READ,
  UNIT_ROW_PRODUCED,
  UNIT_STORAGE_ROW_READ,
  SELECTIVITY_BOOLEAN,
  SELECTIVITY_EQUALITY,
  SELECTIVITY_INEQUALITY,
  SELECTIVITY_MATCH,
  SELECTIVITY_MATCHING,
  SELECTIVITY_NULL_TEST,
  SELECTIVITY_OPAQUE,
  SELECTIVITY_PAIRED_RANGE,
} from "./estimator_constants.ts";

export type Selectivity =
  | { kind: "all" | "zero" | "unique" }
  | { kind: "fraction"; fraction: EstimatorFraction }
  | { kind: "not"; child: Selectivity }
  | { kind: "and" | "or"; lhs: Selectivity; rhs: Selectivity };

export function fractionSelectivity(fraction: EstimatorFraction): Selectivity {
  return { kind: "fraction", fraction };
}

export function andSelectivity(lhs: Selectivity, rhs: Selectivity): Selectivity {
  return { kind: "and", lhs, rhs };
}

export function orSelectivity(lhs: Selectivity, rhs: Selectivity): Selectivity {
  return { kind: "or", lhs, rhs };
}

export function notSelectivity(child: Selectivity): Selectivity {
  return { kind: "not", child };
}

export function saturatingEstimateAdd(a: bigint, b: bigint): bigint {
  const sum = a + b;
  return sum > MAX_ESTIMATE ? MAX_ESTIMATE : sum;
}

export function saturatingEstimateMultiply(a: bigint, b: bigint): bigint {
  if (a < 0n || b < 0n || (a !== 0n && b > MAX_ESTIMATE / a)) return MAX_ESTIMATE;
  return a * b;
}

// Exact ceil(a*b/d). BigInt supplies the cross-core wide intermediate directly.
export function ceilEstimateMultiplyDivide(a: bigint, b: bigint, d: bigint): bigint {
  if (a <= 0n || b <= 0n || d <= 0n) return 0n;
  const quotient = (a * b + d - 1n) / d;
  return quotient > MAX_ESTIMATE ? MAX_ESTIMATE : quotient;
}

// ceil(n*numerator/denominator), deliberately expressed without relying on a wider temporary.
export function scaleEstimateCeil(n: bigint, fraction: EstimatorFraction): bigint {
  if (n <= 0n || fraction.numerator <= 0n) return 0n;
  const quotient = n / fraction.denominator;
  const remainder = n % fraction.denominator;
  const whole = saturatingEstimateMultiply(quotient, fraction.numerator);
  const product = saturatingEstimateMultiply(remainder, fraction.numerator);
  const tail = product / fraction.denominator + (product % fraction.denominator === 0n ? 0n : 1n);
  return saturatingEstimateAdd(whole, tail);
}

function clampEstimate(value: bigint): bigint {
  if (value < 0n) return 0n;
  return value > MAX_ESTIMATE ? MAX_ESTIMATE : value;
}

export function estimateSelectivity(selectivity: Selectivity, inputRows: bigint): bigint {
  const n = clampEstimate(inputRows);
  switch (selectivity.kind) {
    case "all":
      return n;
    case "zero":
      return 0n;
    case "unique":
      return n > 0n ? 1n : 0n;
    case "fraction":
      return scaleEstimateCeil(n, selectivity.fraction);
    case "not":
      return n - estimateSelectivity(selectivity.child, n);
    case "and":
      return estimateSelectivity(selectivity.rhs, estimateSelectivity(selectivity.lhs, n));
    case "or":
      return saturatingEstimateAdd(
        estimateSelectivity(selectivity.lhs, n),
        estimateSelectivity(selectivity.rhs, n),
      ) > n
        ? n
        : saturatingEstimateAdd(
            estimateSelectivity(selectivity.lhs, n),
            estimateSelectivity(selectivity.rhs, n),
          );
  }
}

export function selectivityClass(classification: string): Selectivity {
  switch (classification) {
    case "equality":
      return fractionSelectivity(SELECTIVITY_EQUALITY);
    case "inequality":
      return fractionSelectivity(SELECTIVITY_INEQUALITY);
    case "paired_range":
      return fractionSelectivity(SELECTIVITY_PAIRED_RANGE);
    case "null_test":
      return fractionSelectivity(SELECTIVITY_NULL_TEST);
    case "match":
      return fractionSelectivity(SELECTIVITY_MATCH);
    case "matching":
      return fractionSelectivity(SELECTIVITY_MATCHING);
    case "boolean":
      return fractionSelectivity(SELECTIVITY_BOOLEAN);
    default:
      return fractionSelectivity(SELECTIVITY_OPAQUE);
  }
}

export type CandidateEstimate = {
  rows: bigint;
  units: bigint[];
  cost: bigint;
  tieKey: string;
};

export type PlanEstimate = {
  rows: bigint;
  logicalRows: bigint;
  units: bigint[];
};

export type EstimatedPlan = { root: PlanEstimate; nodes: PlanEstimate[] };

export function emptyPlanEstimate(rows = 0n, logicalRows = rows): PlanEstimate {
  return {
    rows: clampEstimate(rows),
    logicalRows: clampEstimate(logicalRows),
    units: Array<bigint>(ESTIMATOR_UNIT_COUNT).fill(0n),
  };
}

export function clonePlanEstimate(estimate: PlanEstimate): PlanEstimate {
  return { rows: estimate.rows, logicalRows: estimate.logicalRows, units: [...estimate.units] };
}

export function planEstimateCost(estimate: PlanEstimate): bigint {
  let cost = 0n;
  for (let i = 0; i < ESTIMATOR_UNIT_COUNT; i++) {
    cost = saturatingEstimateAdd(
      cost,
      saturatingEstimateMultiply(estimate.units[i]!, ESTIMATOR_UNIT_WEIGHTS[i]!),
    );
  }
  return cost;
}

export function addPlanEstimates(lhs: PlanEstimate, rhs: PlanEstimate): PlanEstimate {
  const out = clonePlanEstimate(lhs);
  for (let i = 0; i < ESTIMATOR_UNIT_COUNT; i++)
    out.units[i] = saturatingEstimateAdd(out.units[i]!, rhs.units[i]!);
  return out;
}

export function repeatPlanEstimate(estimate: PlanEstimate, count: bigint): PlanEstimate {
  const n = clampEstimate(count);
  return {
    rows: saturatingEstimateMultiply(estimate.rows, n),
    logicalRows: saturatingEstimateMultiply(estimate.logicalRows, n),
    units: estimate.units.map((value) => saturatingEstimateMultiply(value, n)),
  };
}

export function addPlanUnit(estimate: PlanEstimate, unit: number, count: bigint): void {
  estimate.units[unit] = saturatingEstimateAdd(estimate.units[unit]!, clampEstimate(count));
}

export function leafEstimatedPlan(estimate: PlanEstimate): EstimatedPlan {
  return { root: estimate, nodes: [estimate] };
}

export function wrapEstimatedPlan(
  child: EstimatedPlan,
  rows: bigint,
  logicalRows: bigint,
  local: bigint[],
): EstimatedPlan {
  const root = clonePlanEstimate(child.root);
  root.rows = clampEstimate(rows);
  root.logicalRows = clampEstimate(logicalRows);
  for (let i = 0; i < ESTIMATOR_UNIT_COUNT; i++)
    root.units[i] = saturatingEstimateAdd(root.units[i]!, local[i] ?? 0n);
  return { root, nodes: [root, ...child.nodes] };
}

export function parentEstimatedPlan(
  root: PlanEstimate,
  ...children: EstimatedPlan[]
): EstimatedPlan {
  const parent = clonePlanEstimate(root);
  return { root: parent, nodes: [parent, ...children.flatMap((child) => child.nodes)] };
}

export type CandidateEstimateInputs = {
  kind: string;
  indexName: string;
  scanRows: bigint;
  outputRows: bigint;
  accessPages: bigint;
  tableHeight: bigint;
  filterNodes: bigint;
  accessWork: bigint;
  producesRows: boolean;
};

export function candidateTieKey(kind: string, indexName: string): string {
  const found = (ESTIMATOR_ACCESS_PATH_ORDER as readonly string[]).indexOf(kind);
  const rank = found < 0 ? ESTIMATOR_ACCESS_PATH_ORDER.length : found;
  return `${rank}:${indexName}`;
}

export function estimateCandidate(input: CandidateEstimateInputs): CandidateEstimate {
  const scanRows = clampEstimate(input.scanRows);
  const outputRows = clampEstimate(input.outputRows);
  const units = Array<bigint>(ESTIMATOR_UNIT_COUNT).fill(0n);
  units[UNIT_STORAGE_ROW_READ] = scanRows;
  units[UNIT_PAGE_READ] = clampEstimate(input.accessPages);
  if (["btree", "gist", "gin", "index_interval"].includes(input.kind)) {
    units[UNIT_PAGE_READ] = saturatingEstimateAdd(
      units[UNIT_PAGE_READ]!,
      saturatingEstimateMultiply(scanRows, clampEstimate(input.tableHeight)),
    );
  }
  units[UNIT_OPERATOR_EVAL] = saturatingEstimateMultiply(
    scanRows,
    clampEstimate(input.filterNodes),
  );
  if (input.producesRows) units[UNIT_ROW_PRODUCED] = outputRows;
  if (input.kind === "gin") units[UNIT_GIN_ENTRY] = clampEstimate(input.accessWork);
  if (input.kind === "gist") units[UNIT_GIST_DESCENT] = clampEstimate(input.accessWork);
  let cost = 0n;
  for (let i = 0; i < ESTIMATOR_UNIT_COUNT; i++) {
    cost = saturatingEstimateAdd(
      cost,
      saturatingEstimateMultiply(units[i]!, ESTIMATOR_UNIT_WEIGHTS[i]!),
    );
  }
  return { rows: outputRows, units, cost, tieKey: candidateTieKey(input.kind, input.indexName) };
}
