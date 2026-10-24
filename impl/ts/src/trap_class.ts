// The planner's safe-conjunct gate (spec/design/planner.md §3.5) and the widened set-operation arm
// rewrite (§3.6). Which casts and functions cannot trap is catalog data (functions.md §15):
// casts.toml and catalog.toml classify them, and the codegen emits NON_TRAPPING_CASTS and the
// operators' traps / preservesEquality fields. This module only reads that data.

import type { QueryPlan, RExpr, ResolvedType } from "./executor.ts";
import {
  estimatorIsNull,
  estimatorLiteral,
  estimatorValue,
  resolvedTypeEqual,
} from "./executor.ts";
import { NON_TRAPPING_CASTS, OPERATORS } from "./operators.ts";
import type { ScalarType } from "./types.ts";

// TrapClass is the trap classification of a cast or function (functions.md §15): "may" raises (the
// default, for anything unclassified); "none" raises nothing and gives equal results for equal
// arguments; "lossy" raises nothing but may separate equal arguments (a decimal or float render:
// 1.5 = 1.50, -0 = 0).
export type TrapClass = "may" | "none" | "lossy";

function trapClassOf(preservesEquality: boolean): TrapClass {
  return preservesEquality ? "none" : "lossy";
}

// resolvedScalarName is the casts.toml name of a scalar resolved type (null for anything else).
function resolvedScalarName(t: ResolvedType): string | null {
  switch (t.kind) {
    case "int":
    case "float":
      return t.ty;
    case "decimal":
      return "decimal";
    case "bool":
      return "boolean";
    case "text":
      return "text";
    case "uuid":
      return "uuid";
    default:
      return null;
  }
}

// castTrapClass classifies a cast from a value of type from to target (casts.toml). A numeric(p,s)
// typmod can overflow, so such a cast may trap whatever its row says.
export function castTrapClass(from: ResolvedType, target: ScalarType, typmod: unknown): TrapClass {
  if (typmod !== null && typmod !== undefined) return "may";
  const name = resolvedScalarName(from);
  if (name === null) return "may";
  const row = NON_TRAPPING_CASTS.find((c) => c.from === name && c.to === target);
  return row === undefined ? "may" : trapClassOf(row.preservesEquality);
}

// FUNCTION_TRAPS maps each catalog function name to its (name-level) trap classification.
const FUNCTION_TRAPS: ReadonlyMap<string, TrapClass> = (() => {
  const m = new Map<string, TrapClass>();
  for (const op of OPERATORS) {
    if (op.kind !== "function") continue;
    let c: TrapClass = op.traps ? "may" : trapClassOf(op.preservesEquality);
    // verify.rb forbids disagreeing overloads; stay conservative regardless.
    const prev = m.get(op.name);
    if (prev !== undefined && prev !== c) c = "may";
    m.set(op.name, c);
  }
  return m;
})();

// nodeTrapClass is a value node's own trap classification, ignoring its children: "none" for a
// column, parameter, or literal; the cast's recorded class; a function's catalog class (looked up by
// its catalog name — the classification is name-level).
function nodeTrapClass(e: RExpr): TrapClass {
  switch (e.kind) {
    case "column":
    case "param":
      return "none";
    case "cast":
      return e.trap ?? "may";
    case "scalarFunc":
      return FUNCTION_TRAPS.get(e.func) ?? "may";
    case "casing":
      return FUNCTION_TRAPS.get(e.upper ? "upper" : "lower") ?? "may";
    default:
      return estimatorLiteral(e) ? "none" : "may";
  }
}

// valueChildren is a classified value node's children (the only node kinds nonTrapping admits).
function valueChildren(e: RExpr): RExpr[] {
  switch (e.kind) {
    case "cast":
      return [e.operand];
    case "scalarFunc":
      return e.args;
    case "casing":
      return [e.arg];
    default:
      return [];
  }
}

// nonTrapping reports whether a value expression can raise no error (planner.md §3.5): a column,
// literal, or parameter, or a classified cast or function call over non-trapping arguments.
export function nonTrapping(e: RExpr): boolean {
  return nodeTrapClass(e) !== "may" && valueChildren(e).every(nonTrapping);
}

// conjunctLossy reports whether a pushdown-safe conjunct contains a cast or function that may give
// different results for equal values, which keeps it out of bodies that match rows (planner.md
// §3.2). Only the gate's node kinds reach it.
export function conjunctLossy(e: RExpr): boolean {
  switch (e.kind) {
    case "and":
    case "or":
    case "compare":
    case "distinct":
    case "like":
      return conjunctLossy(e.lhs) || conjunctLossy(e.rhs);
    case "not":
    case "isNull":
      return conjunctLossy(e.operand);
    case "cast":
    case "scalarFunc":
    case "casing":
      return nodeTrapClass(e) === "lossy" || valueChildren(e).some(conjunctLossy);
    default:
      return false;
  }
}

// likePatternProven is the plan-time LIKE pattern proof (planner.md §3.5): the pattern is a
// plan-time literal (a literal, or a parameter read through the custom-plan frame) that is NULL or
// text whose escape characters all pair, so the matcher can never raise 22025.
export function likePatternProven(p: RExpr): boolean {
  if (!estimatorLiteral(p)) return false;
  if (estimatorIsNull(p)) return true;
  const v = estimatorValue(p);
  return v !== null && v.kind === "constText" && likePatternWellFormed(v.value);
}

// likePatternWellFormed reports whether every `\` in a LIKE pattern, read left to right in pairs, is
// followed by another character — the only condition under which likeMatch raises 22025.
function likePatternWellFormed(pattern: string): boolean {
  const p = Array.from(pattern);
  for (let i = 0; i < p.length; i++) {
    if (p[i] === "\\") {
      if (i + 1 >= p.length) return false;
      i++;
    }
  }
  return true;
}

// armWidening decides how a set-operation arm column of type leaf reaches the unified output type
// want (planner.md §3.6): "noop" with no value change, a widening-cast target, or null (not at all).
export function armWidening(leaf: ResolvedType, want: ResolvedType): "noop" | ScalarType | null {
  if (valueNoOpWidening(leaf, want)) return "noop";
  if (leaf.kind === "int" && want.kind === "decimal") return "decimal";
  if (leaf.kind === "float" && leaf.ty === "f32" && want.kind === "float" && want.ty === "f64") {
    return "f64";
  }
  return null;
}

// valueNoOpWidening reports whether the set operation's conversion from leaf to want leaves every
// value unchanged: the same type, integer widths (alone or inside arrays and anonymous records), or
// a NULL-typed column.
function valueNoOpWidening(leaf: ResolvedType, want: ResolvedType): boolean {
  if (resolvedTypeEqual(leaf, want) || leaf.kind === "null") return true;
  if (leaf.kind === "int" && want.kind === "int") return true;
  if (leaf.kind === "array" && want.kind === "array")
    return valueNoOpWidening(leaf.elem, want.elem);
  if (
    leaf.kind === "composite" &&
    want.kind === "composite" &&
    leaf.name === null &&
    want.name === null
  ) {
    return (
      leaf.fields.length === want.fields.length &&
      leaf.fields.every((f, i) => valueNoOpWidening(f.type, want.fields[i]!.type))
    );
  }
  return false;
}

// nonMatchingBody reports whether a body never matches rows by value (planner.md §3.2): every
// SELECT is ungrouped with no DISTINCT and no window function, combined only by UNION ALL. A lossy
// conjunct may move only into such a body.
export function nonMatchingBody(q: QueryPlan): boolean {
  switch (q.kind) {
    case "select":
      return !q.isAgg && !q.distinct && q.distinctOn === 0 && !q.hasWindow;
    case "setOp":
      return q.op === "union" && q.all && nonMatchingBody(q.lhs) && nonMatchingBody(q.rhs);
    default:
      return false;
  }
}
