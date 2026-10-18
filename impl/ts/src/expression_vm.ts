// Internal, opt-in experiment; no SQL setting or embedding API. See
// spec/design/expression-vm-experiment.md. Programs are immutable and cached by resolved-tree
// identity; scratch belongs to an execution environment, never to a cached plan.
import type { BinaryOp } from "./ast.ts";
import type { Collation } from "./collation.ts";
import type { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import { evalArith, evalCast, evalCompareValues, evalNegValue, operatorCost } from "./eval.ts";
import type { EvalEnv, RExpr } from "./executor.ts";
import { evalDateArith, typeError } from "./executor.ts";
import { resolveUnfetchedSelf } from "./format.ts";
import type { Row } from "./storage.ts";
import { isInteger, type ScalarType } from "./types.ts";
import {
  boolAnd,
  boolNot,
  boolOr,
  boolValue,
  intValue,
  isNullTest,
  isTrue,
  nullValue,
  type Value,
} from "./value.ts";

type Instruction =
  | { op: "enter"; cost: bigint }
  | { op: "constant"; dst: number; value: Value }
  | { op: "column" | "param"; dst: number; index: number }
  | { op: "neg" | "cast"; dst: number; width: ScalarType }
  | { op: "not" | "truth"; dst: number }
  | { op: "isNull"; dst: number; negated: boolean }
  | { op: "arith"; dst: number; rhs: number; operator: BinaryOp; width: ScalarType }
  | {
      op: "compare";
      dst: number;
      rhs: number;
      operator: BinaryOp;
      collation: Collation | null;
    }
  | { op: "and" | "or"; dst: number; rhs: number }
  | { op: "false" | "true" | "notTrue" | "notNull"; src: number; target: number }
  | { op: "jump"; target: number };

export type ExpressionProgram = {
  readonly instructions: readonly Instruction[];
  readonly slots: number;
};

let enabled = false;
let treeDepth = 0;
let valueCache = new WeakMap<RExpr, ExpressionProgram | null>();
let filterCache = new WeakMap<RExpr, ExpressionProgram | null>();
const scratch = new WeakMap<EvalEnv, { values: Value[]; active: boolean }>();

// Counts describe compiled representations, not a host-heap byte estimate. Compilation timing is
// measured separately by the experiment driver, keeping clock calls out of the evaluator.
export const expressionVmStats = { compiled: 0, fallbacks: 0, instructions: 0, slots: 0 };

export function resetExpressionVmStats(clearPrograms = false): void {
  expressionVmStats.compiled = 0;
  expressionVmStats.fallbacks = 0;
  expressionVmStats.instructions = 0;
  expressionVmStats.slots = 0;
  if (clearPrograms) {
    valueCache = new WeakMap();
    filterCache = new WeakMap();
  }
}

// A synchronous scope: the experiment driver drains cursors inside it. Deliberately not exported
// from lib.ts/tooling.ts and never inferred from environment variables by the engine.
export function withExpressionVm<T>(useVm: boolean, run: () => T): T {
  const before = enabled;
  enabled = useVm;
  try {
    return run();
  } finally {
    enabled = before;
  }
}

export function expressionVmEnabled(): boolean {
  return enabled && treeDepth === 0;
}

export function withTreeEvaluation<T>(run: () => T): T {
  treeDepth++;
  try {
    return run();
  } finally {
    treeDepth--;
  }
}

// Independent, handwritten lowering. Register zero holds the result. A binary's right child uses
// one scratch slot, released after its instruction, so siblings reuse storage. Branch targets are
// patched once before publishing the program. An unsupported descendant rejects the whole root.
export function compileExpressionVm(root: RExpr, filter = false): ExpressionProgram | null {
  const instructions: Instruction[] = [];
  let slots = 1;
  let next = 1;
  function temporary(): number {
    const slot = next++;
    slots = Math.max(slots, next);
    return slot;
  }
  function lower(e: RExpr, dst: number, asFilter = false): boolean {
    if (asFilter && e.kind !== "and") {
      if (!lower(e, dst)) return false;
      instructions.push({ op: "truth", dst });
      return true;
    }
    switch (e.kind) {
      case "column":
      case "param":
        instructions.push({ op: e.kind, dst, index: e.index });
        return true;
      case "constInt":
        instructions.push({ op: "constant", dst, value: intValue(e.value) });
        return true;
      case "constBool":
        instructions.push({ op: "constant", dst, value: boolValue(e.value) });
        return true;
      case "constNull":
        instructions.push({ op: "constant", dst, value: nullValue() });
        return true;
      case "neg":
      case "cast":
      case "not":
      case "isNull":
        if (e.kind === "neg" && !isInteger(e.result)) return false;
        if (e.kind === "cast" && !isInteger(e.target)) return false;
        instructions.push({
          op: "enter",
          cost:
            e.kind === "isNull" || e.kind === "cast" ? COSTS.operatorEval : operatorCost(e.kind),
        });
        if (!lower(e.operand, dst)) return false;
        if (e.kind === "neg") instructions.push({ op: "neg", dst, width: e.result });
        else if (e.kind === "cast") instructions.push({ op: "cast", dst, width: e.target });
        else if (e.kind === "isNull") instructions.push({ op: "isNull", dst, negated: e.negated });
        else instructions.push({ op: "not", dst });
        return true;
      case "arith":
      case "compare": {
        if (e.kind === "arith" && !isInteger(e.result)) return false;
        instructions.push({ op: "enter", cost: operatorCost(e.op) });
        if (!lower(e.lhs, dst)) return false;
        const rhs = temporary();
        if (!lower(e.rhs, rhs)) return false;
        if (e.kind === "arith")
          instructions.push({ op: "arith", dst, rhs, operator: e.op, width: e.result });
        else instructions.push({ op: "compare", dst, rhs, operator: e.op, collation: e.collation });
        next--;
        return true;
      }
      case "and":
      case "or": {
        instructions.push({ op: "enter", cost: operatorCost(e.kind) });
        if (!lower(e.lhs, dst, asFilter)) return false;
        const branch: Instruction = {
          op: e.kind === "and" ? "false" : "true",
          src: dst,
          target: 0,
        };
        instructions.push(branch);
        const rhs = temporary();
        if (!lower(e.rhs, rhs, asFilter)) return false;
        instructions.push({ op: e.kind, dst, rhs });
        next--;
        branch.target = instructions.length;
        return true;
      }
      case "case": {
        if (e.coerceDecimal) return false;
        instructions.push({ op: "enter", cost: COSTS.operatorEval });
        const ends: Extract<Instruction, { op: "jump" }>[] = [];
        for (const arm of e.arms) {
          const cond = temporary();
          if (!lower(arm.cond, cond)) return false;
          const skip: Instruction = { op: "notTrue", src: cond, target: 0 };
          instructions.push(skip);
          next--;
          if (!lower(arm.result, dst)) return false;
          const end: Instruction = { op: "jump", target: 0 };
          instructions.push(end);
          ends.push(end);
          skip.target = instructions.length;
        }
        if (!lower(e.els, dst)) return false;
        for (const end of ends) end.target = instructions.length;
        return true;
      }
      case "coalesce": {
        if (e.coerceDecimal || e.args.length === 0) return false;
        instructions.push({ op: "enter", cost: COSTS.operatorEval });
        const ends: Extract<Instruction, { src: number }>[] = [];
        for (let i = 0; i < e.args.length; i++) {
          if (!lower(e.args[i], dst)) return false;
          if (i + 1 !== e.args.length) {
            const end: Instruction = { op: "notNull", src: dst, target: 0 };
            instructions.push(end);
            ends.push(end);
          }
        }
        for (const end of ends) end.target = instructions.length;
        return true;
      }
      default:
        return false;
    }
  }
  return lower(root, 0, filter) ? { instructions, slots } : null;
}

// The only mutable machine state is the execution-owned register array. A nested evaluation on
// the same env gets private scratch. Finally clears every retained value, including error exits,
// so a cursor/prepared plan cannot retain its last row's values through the scratch cache.
export function runExpressionVm(
  program: ExpressionProgram,
  row: Row,
  env: EvalEnv,
  meter: Meter,
): Value {
  let state = scratch.get(env);
  if (state === undefined) {
    state = { values: [], active: false };
    scratch.set(env, state);
  }
  const owned = !state.active;
  const values = owned ? state.values : [];
  if (owned) state.active = true;
  try {
    const code = program.instructions;
    for (let pc = 0; pc < code.length; pc++) {
      const inst = code[pc];
      switch (inst.op) {
        case "enter":
          meter.guard();
          meter.charge(inst.cost);
          break;
        case "constant":
          meter.guard();
          values[inst.dst] = inst.value;
          break;
        case "column": {
          meter.guard();
          const value = row[inst.index];
          values[inst.dst] = value.kind === "unfetched" ? resolveUnfetchedSelf(value.ref) : value;
          break;
        }
        case "param":
          meter.guard();
          values[inst.dst] = env.params[inst.index];
          break;
        case "neg":
          values[inst.dst] = evalNegValue(inst.width, values[inst.dst]);
          break;
        case "cast":
          if (values[inst.dst].kind !== "null")
            values[inst.dst] = evalCast(values[inst.dst], inst.width, null);
          break;
        case "not":
          values[inst.dst] = boolNot(values[inst.dst]);
          break;
        case "truth":
          values[inst.dst] = boolValue(isTrue(values[inst.dst]));
          break;
        case "isNull":
          values[inst.dst] = boolValue(isNullTest(values[inst.dst], inst.negated));
          break;
        case "arith": {
          const lhs = values[inst.dst];
          const rhs = values[inst.rhs];
          if (lhs.kind === "null" || rhs.kind === "null") values[inst.dst] = nullValue();
          // A date difference also resolves to i32, while column nodes carry no source type.
          // Keep the tree evaluator's source-family dispatch before the integer-only kernel.
          else if (lhs.kind === "date" || rhs.kind === "date")
            values[inst.dst] = evalDateArith(inst.operator, lhs, rhs, inst.width);
          else {
            if (lhs.kind !== "int" || rhs.kind !== "int")
              throw typeError("internal: non-integer arithmetic");
            values[inst.dst] = evalArith(inst.operator, lhs.int, rhs.int, inst.width);
          }
          break;
        }
        case "compare":
          values[inst.dst] = evalCompareValues(
            inst.operator,
            inst.collation,
            values[inst.dst],
            values[inst.rhs],
            meter,
          );
          break;
        case "and":
          values[inst.dst] = boolAnd(values[inst.dst], values[inst.rhs]);
          break;
        case "or":
          values[inst.dst] = boolOr(values[inst.dst], values[inst.rhs]);
          break;
        case "false": {
          const value = values[inst.src];
          if (value.kind === "bool" && !value.value) pc = inst.target - 1;
          break;
        }
        case "true":
          if (isTrue(values[inst.src])) pc = inst.target - 1;
          break;
        case "notTrue":
          if (!isTrue(values[inst.src])) pc = inst.target - 1;
          break;
        case "notNull":
          if (values[inst.src].kind !== "null") pc = inst.target - 1;
          break;
        case "jump":
          pc = inst.target - 1;
          break;
      }
    }
    return values[0];
  } finally {
    values.fill(nullValue());
    if (owned) state.active = false;
  }
}

export function tryExpressionVm(
  expr: RExpr,
  row: Row,
  env: EvalEnv,
  meter: Meter,
  filter: boolean,
): Value | undefined {
  const cache = filter ? filterCache : valueCache;
  let program = cache.get(expr);
  if (program === undefined) {
    program = compileExpressionVm(expr, filter);
    cache.set(expr, program);
    if (program === null) expressionVmStats.fallbacks++;
    else {
      expressionVmStats.compiled++;
      expressionVmStats.instructions += program.instructions.length;
      expressionVmStats.slots += program.slots;
    }
  }
  return program === null ? undefined : runExpressionVm(program, row, env, meter);
}
