// Plan-time constant folding (spec/design/planner.md §3.7). A resolved expression's maximal closed
// subtrees — an admitted interior node whose every operand is already a constant — are evaluated
// once, when the expression is resolved, and replaced by the constant they produce. The boolean
// connectives additionally drop a constant operand whose value cannot change the result.
//
// The pass is results-identical and never raises: an evaluation that fails (an error, the fold
// meter's ceiling) leaves its subtree as written, so the error still surfaces only where execution
// reaches it. It reads no row, parameter, session setting, clock, entropy, or catalog state: the
// admitted node kinds and value domain below exclude every kernel that could.

import { Meter } from "./cost.ts";
import { EngineError } from "./errors.ts";
import { evalExpr } from "./eval.ts";
import { type EvalEnv, type RExpr, valueToRExpr } from "./executor.ts";
import type { Value } from "./value.ts";

// FOLD_COST_LIMIT bounds one fold attempt's evaluation work (planner.md §3.7). An attempt that
// reaches it leaves its subtree unfolded.
const FOLD_COST_LIMIT = 10000n;

// foldConstants folds e and returns the folded expression. `env` evaluates a closed subtree: it
// binds no parameters and no outer rows.
export function foldConstants(e: RExpr, env: EvalEnv): RExpr {
  foldChildren(e, env);
  if (!foldAdmitted(e)) return e;
  if (foldOperandsConstant(e)) {
    let v: Value | null = null;
    try {
      v = evalExpr(e, [], env, new Meter(FOLD_COST_LIMIT));
    } catch (err) {
      if (!(err instanceof EngineError)) throw err;
      v = null;
    }
    if (v !== null && foldValueInDomain(v)) return valueToRExpr(v);
  }
  return foldConnective(e);
}

// foldChildren folds every child of e (bottom-up) in place. A subquery body is resolved — and
// folded — by its own planning; only its IN operand is walked here.
function foldChildren(e: RExpr, env: EvalEnv): void {
  const f = (x: RExpr): RExpr => foldConstants(x, env);
  switch (e.kind) {
    case "subquery":
      if (e.lhs !== null) e.lhs = f(e.lhs);
      return;
    case "cast":
    case "arrayCast":
    case "compositeCast":
    case "neg":
    case "not":
    case "isNull":
    case "isJson":
    case "jsonCtor":
      e.operand = f(e.operand);
      return;
    case "arith":
    case "compare":
    case "and":
    case "or":
    case "distinct":
    case "like":
    case "regex":
      e.lhs = f(e.lhs);
      e.rhs = f(e.rhs);
      return;
    case "jsonGet":
    case "jsonHasKey":
    case "jsonDelete":
      e.base = f(e.base);
      e.arg = f(e.arg);
      return;
    case "jsonContains":
    case "jsonConcat":
      e.a = f(e.a);
      e.b = f(e.b);
      return;
    case "casing":
      e.arg = f(e.arg);
      return;
    case "atTimeZone":
      e.zone = f(e.zone);
      e.value = f(e.value);
      return;
    case "dateTrunc":
      e.unit = f(e.unit);
      e.value = f(e.value);
      if (e.zone) e.zone = f(e.zone);
      return;
    case "extract":
      e.value = f(e.value);
      return;
    case "dateConvert":
      e.inner = f(e.inner);
      return;
    case "case":
      e.arms = e.arms.map((arm) => ({ cond: f(arm.cond), result: f(arm.result) }));
      e.els = f(e.els);
      return;
    case "greatestLeast":
    case "coalesce":
    case "nullIf":
    case "scalarFunc":
    case "hostFunc":
    case "arrayFunc":
    case "regexFunc":
    case "rangeFunc":
    case "rangeCtor":
    case "rangeOp":
    case "rangeSetOp":
    case "variadic":
    case "jsonBuild":
    case "jsonSetInsert":
    case "jsonObjectFromArrays":
    case "jsonImage":
    case "jsonPathFn":
    case "jsonSqlFn":
      e.args = e.args.map(f);
      return;
    case "row":
      e.fields = e.fields.map(f);
      return;
    case "array":
      e.elements = e.elements.map(f);
      return;
    case "field":
      e.base = f(e.base);
      return;
    case "subscript":
      e.base = f(e.base);
      e.subscripts = e.subscripts.map((s) =>
        s.isSlice
          ? {
              isSlice: true,
              lower: s.lower === null ? null : f(s.lower),
              upper: s.upper === null ? null : f(s.upper),
            }
          : { isSlice: false, index: f(s.index) },
      );
      return;
    case "inValues":
      e.lhs = f(e.lhs);
      return;
    case "quantified":
      e.lhs = f(e.lhs);
      e.array = f(e.array);
      return;
    case "widthBucketArray":
      e.operand = f(e.operand);
      e.thresholds = f(e.thresholds);
      return;
    default:
      return; // leaves: column, outerColumn, param, const*, dateClock
  }
}

// foldConnective drops a constant operand of AND/OR that cannot change the value (planner.md §3.7):
// a left operand that decides the connective replaces it (the right operand is never evaluated, at
// run time either); a left operand that does not decide it leaves the right operand; a right
// operand that is the identity leaves the left. Each is exact under three-valued logic.
function foldConnective(e: RExpr): RExpr {
  if (e.kind !== "and" && e.kind !== "or") return e;
  const decides = e.kind === "or"; // TRUE decides OR, FALSE decides AND
  if (e.lhs.kind === "constBool") {
    return e.lhs.value === decides ? { kind: "constBool", value: decides } : e.rhs;
  }
  if (e.rhs.kind === "constBool" && e.rhs.value !== decides) return e.lhs;
  return e;
}

// foldAdmitted is the admitted interior node set (planner.md §3.7).
function foldAdmitted(e: RExpr): boolean {
  switch (e.kind) {
    case "cast":
    case "neg":
    case "not":
    case "arith":
    case "compare":
    case "and":
    case "or":
    case "isNull":
    case "distinct":
    case "case":
    case "coalesce":
    case "greatestLeast":
    case "array":
      return true;
    default:
      return false;
  }
}

// foldOperandsConstant reports whether every operand of an admitted node is a constant in the fold
// value domain.
function foldOperandsConstant(e: RExpr): boolean {
  switch (e.kind) {
    case "cast":
    case "neg":
    case "not":
    case "isNull":
      return foldConstant(e.operand);
    case "arith":
    case "compare":
    case "and":
    case "or":
    case "distinct":
      return foldConstant(e.lhs) && foldConstant(e.rhs);
    case "case":
      return (
        e.arms.every((arm) => foldConstant(arm.cond) && foldConstant(arm.result)) &&
        foldConstant(e.els)
      );
    case "coalesce":
    case "greatestLeast":
      return e.args.every(foldConstant);
    case "array":
      return e.elements.every(foldConstant);
    default:
      return false;
  }
}

// foldConstant reports whether e is a constant node in the fold value domain.
function foldConstant(e: RExpr): boolean {
  switch (e.kind) {
    case "constNull":
    case "constInt":
    case "constBool":
    case "constText":
    case "constDecimal":
    case "constBytea":
    case "constUuid":
    case "constFloat":
      return true;
    case "constArray":
      return foldValueInDomain(e.value);
    default:
      return false;
  }
}

// foldValueInDomain is the fold value domain (planner.md §3.7): NULL, the integers, decimal,
// f32/f64, boolean, text, bytea, uuid, and arrays of those.
function foldValueInDomain(v: Value): boolean {
  switch (v.kind) {
    case "null":
    case "int":
    case "bool":
    case "text":
    case "decimal":
    case "bytea":
    case "uuid":
    case "f32":
    case "f64":
      return true;
    case "array":
      return v.elements.every((el) => el.kind !== "array" && foldValueInDomain(el));
    default:
      return false;
  }
}
