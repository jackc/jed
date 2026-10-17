// The partial-index predicate-implication prover (spec/design/indexes.md §9.1): does a query's WHERE
// (a list of filter expressions, each an AND-chain) imply a partial index's predicate? A
// deterministic, bounded subset of PostgreSQL's `predtest.c`. Sound-if-incomplete: false means "not
// proven", which only costs the index (a correct full scan + residual filter). Mirrors
// impl/rust/src/executor/implication.rs.
//
// The filter is in GLOBAL column coordinates; the predicate is table-local and is matched shifted by
// `offset` (the relation's global column base), exactly as rexprEqShifted does.

import type { BinaryOp } from "./ast.ts";
import type { Decimal } from "./decimal.ts";
import { flipCmp, type RExpr, rexprEqShifted } from "./executor.ts";
import { compareBytes } from "./pmap.ts";
import { compareTextC, type Value } from "./value.ts";

// The prover's step budget (indexes.md §9.1): every invocation of one of its three procedures
// consumes one step, and an exhausted budget answers "not proven".
const IMPLICATION_STEP_BUDGET = 10_000;
// The largest `x = ANY(array)` premise the prover expands (PostgreSQL's MAX_SAOP_ARRAY_SIZE).
const IMPLICATION_MAX_ARRAY = 100;

type CmpOp = "eq" | "ne" | "lt" | "le" | "gt" | "ge";

function asCmpOp(op: BinaryOp): CmpOp | null {
  switch (op) {
    case "eq":
    case "ne":
    case "lt":
    case "le":
    case "gt":
    case "ge":
      return op;
    default:
      return null;
  }
}

// filtersImplyPredicate: does the conjunction of filters imply the partial-index predicate pred
// (indexes.md §9.1)?
export function filtersImplyPredicate(filters: RExpr[], pred: RExpr, offset: number): boolean {
  const conjuncts: RExpr[] = [];
  for (const f of filters) flattenAnd(f, conjuncts);
  const prover = new Prover(offset);
  return prover.impliedByConjuncts(conjuncts, pred);
}

// filterImpliesPredicate is the single-filter form of filtersImplyPredicate.
export function filterImpliesPredicate(filter: RExpr, pred: RExpr, offset: number): boolean {
  return filtersImplyPredicate([filter], pred, offset);
}

// predicateRejectsNull reports whether pred (table-local) is NULL-rejecting (indexes.md §9.3): never
// TRUE for a row whose every column is NULL, as for a NULL-extended outer-join row. A syntactic,
// conservative test.
export function predicateRejectsNull(pred: RExpr): boolean {
  const bare = (e: RExpr): boolean => e.kind === "column";
  switch (pred.kind) {
    case "and":
      return predicateRejectsNull(pred.lhs) || predicateRejectsNull(pred.rhs);
    case "or":
      return predicateRejectsNull(pred.lhs) && predicateRejectsNull(pred.rhs);
    case "compare":
      return bare(pred.lhs) || bare(pred.rhs);
    case "isNull":
      return pred.negated && bare(pred.operand);
    case "column":
      return true;
    case "not":
      return bare(pred.operand);
    case "quantified":
      return !pred.all && bare(pred.lhs);
    case "like":
    case "regex":
      return bare(pred.lhs);
    default:
      return false;
  }
}

function flattenAnd(e: RExpr, out: RExpr[]): void {
  if (e.kind === "and") {
    flattenAnd(e.lhs, out);
    flattenAnd(e.rhs, out);
  } else {
    out.push(e);
  }
}

// A comparison constant the prover can order: one scalar family, compared within its own variant.
type ProverConst =
  | { kind: "int"; v: bigint }
  | { kind: "text"; v: string }
  | { kind: "decimal"; v: Decimal }
  | { kind: "date"; v: bigint }
  | { kind: "timestamp"; v: bigint }
  | { kind: "timestamptz"; v: bigint }
  | { kind: "bool"; v: boolean }
  | { kind: "uuid"; v: Uint8Array };

function constFromRExpr(e: RExpr): ProverConst | null {
  switch (e.kind) {
    case "constInt":
      return { kind: "int", v: e.value };
    case "constText":
      return { kind: "text", v: e.value };
    case "constDecimal":
      return { kind: "decimal", v: e.value };
    case "constDate":
      return { kind: "date", v: e.value };
    case "constTimestamp":
      return { kind: "timestamp", v: e.value };
    case "constTimestamptz":
      return { kind: "timestamptz", v: e.value };
    case "constBool":
      return { kind: "bool", v: e.value };
    case "constUuid":
      return { kind: "uuid", v: e.value };
    default:
      return null;
  }
}

function constFromValue(v: Value): ProverConst | null {
  switch (v.kind) {
    case "int":
      return { kind: "int", v: v.int };
    case "text":
      return { kind: "text", v: v.text };
    case "decimal":
      return { kind: "decimal", v: v.dec };
    case "date":
      return { kind: "date", v: v.days };
    case "timestamp":
      return { kind: "timestamp", v: v.micros };
    case "timestamptz":
      return { kind: "timestamptz", v: v.micros };
    case "bool":
      return { kind: "bool", v: v.value };
    case "uuid":
      return { kind: "uuid", v: v.bytes };
    default:
      return null;
  }
}

function sign(n: number): number {
  return n < 0 ? -1 : n > 0 ? 1 : 0;
}

function cmpBig(a: bigint, b: bigint): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

// The order of two constants of the SAME family (-1/0/1); null across families (never compared).
function constCmp(a: ProverConst, b: ProverConst): number | null {
  if (a.kind !== b.kind) return null;
  switch (a.kind) {
    case "int":
    case "date":
    case "timestamp":
    case "timestamptz":
      return cmpBig(a.v, b.v as bigint);
    case "text":
      return sign(compareTextC(a.v, b.v as string));
    case "decimal":
      return sign(a.v.cmpValue(b.v as Decimal));
    case "bool": {
      const x = a.v ? 1 : 0;
      const y = (b.v as boolean) ? 1 : 0;
      return x - y;
    }
    case "uuid":
      return sign(compareBytes(a.v, b.v as Uint8Array));
  }
}

// A premise atom `operand op value` known TRUE for every row the filter admits. cOrder is whether the
// source comparison carried no collation (a C/byte or non-text comparison) — only then may an
// ordering operator take part in a comparison implication.
type Premise = { operand: RExpr; op: CmpOp; value: ProverConst; cOrder: boolean };

// Normalize w to `operand op const` when it is a comparison with a non-NULL prover constant on one
// side (the operator flipped when the constant is on the left).
function comparisonAtom(w: RExpr): Premise | null {
  if (w.kind !== "compare") return null;
  const op = asCmpOp(w.op);
  if (op === null) return null;
  const cOrder = w.collation === null;
  const r = constFromRExpr(w.rhs);
  if (r !== null) return { operand: w.lhs, op, value: r, cOrder };
  const l = constFromRExpr(w.lhs);
  if (l !== null) return { operand: w.rhs, op: flipCmp(op) as CmpOp, value: l, cOrder };
  return null;
}

// Does `x op1 c1` (TRUE) imply `x op2 c2`, given ord = cmp(c1, c2)? PostgreSQL's btree implication
// table (predtest.c BT_implic_table), over a total order.
function comparisonImplies(op1: CmpOp, op2: CmpOp, ord: number): boolean {
  switch (op1) {
    case "eq":
      switch (op2) {
        case "eq":
          return ord === 0;
        case "ne":
          return ord !== 0;
        case "lt":
          return ord < 0;
        case "le":
          return ord <= 0;
        case "gt":
          return ord > 0;
        case "ge":
          return ord >= 0;
      }
      return false;
    case "lt":
      return (op2 === "lt" || op2 === "le" || op2 === "ne") && ord <= 0;
    case "le":
      if (op2 === "lt" || op2 === "ne") return ord < 0;
      if (op2 === "le") return ord <= 0;
      return false;
    case "gt":
      return (op2 === "gt" || op2 === "ge" || op2 === "ne") && ord >= 0;
    case "ge":
      if (op2 === "gt" || op2 === "ne") return ord > 0;
      if (op2 === "ge") return ord >= 0;
      return false;
    case "ne":
      return op2 === "ne" && ord === 0;
  }
}

// `x = ANY(array)` over a constant array of at most IMPLICATION_MAX_ARRAY elements, as its operand
// and the elements (null for a NULL element — a NULL never makes the ANY TRUE; any other non-prover
// element makes the whole premise unusable), plus whether the quantifier carried no collation.
function anyArrayPremise(
  w: RExpr,
): { operand: RExpr; elems: (ProverConst | null)[]; cOrder: boolean } | null {
  if (w.kind !== "quantified" || w.op !== "eq" || w.all || w.rowWise) return null;
  const out: (ProverConst | null)[] = [];
  const array = w.array;
  if (array.kind === "constArray") {
    if (array.value.kind !== "array") return null;
    const elements = array.value.elements;
    if (elements.length > IMPLICATION_MAX_ARRAY) return null;
    for (const v of elements) {
      if (v.kind === "null") {
        out.push(null);
      } else {
        const c = constFromValue(v);
        if (c === null) return null;
        out.push(c);
      }
    }
  } else if (array.kind === "array" && !array.nested) {
    if (array.elements.length > IMPLICATION_MAX_ARRAY) return null;
    for (const e of array.elements) {
      if (e.kind === "constNull") {
        out.push(null);
      } else {
        const c = constFromRExpr(e);
        if (c === null) return null;
        out.push(c);
      }
    }
  } else {
    return null;
  }
  return { operand: w.lhs, elems: out, cOrder: w.collation === null };
}

class Prover {
  private readonly offset: number;
  private steps: number;

  constructor(offset: number) {
    this.offset = offset;
    this.steps = IMPLICATION_STEP_BUDGET;
  }

  private tick(): boolean {
    if (this.steps === 0) return false;
    this.steps--;
    return true;
  }

  // Procedure 1 — the filter's top-level conjunct list ws implies p.
  impliedByConjuncts(ws: RExpr[], p: RExpr): boolean {
    if (!this.tick()) return false;
    if (p.kind === "and") {
      return this.impliedByConjuncts(ws, p.lhs) && this.impliedByConjuncts(ws, p.rhs);
    }
    for (const w of ws) {
      if (this.clauseImplies(w, p)) return true;
    }
    if (p.kind === "or") {
      return this.impliedByConjuncts(ws, p.lhs) || this.impliedByConjuncts(ws, p.rhs);
    }
    return false;
  }

  // Procedure 2 — one filter clause w implies p.
  private clauseImplies(w: RExpr, p: RExpr): boolean {
    if (!this.tick()) return false;
    if (rexprEqShifted(w, p, this.offset)) return true;
    if (p.kind === "and") return this.clauseImplies(w, p.lhs) && this.clauseImplies(w, p.rhs);
    if (w.kind === "or") return this.clauseImplies(w.lhs, p) && this.clauseImplies(w.rhs, p);
    if (w.kind === "and") return this.clauseImplies(w.lhs, p) || this.clauseImplies(w.rhs, p);
    const anyPremise = anyArrayPremise(w);
    if (anyPremise !== null) {
      // `x = ANY(array)` is TRUE only when x equals some non-NULL element, so it implies p iff every
      // non-NULL element's `x = e` does (and at least one element is non-NULL).
      let any = false;
      for (const value of anyPremise.elems) {
        if (value === null) continue;
        any = true;
        const premise: Premise = {
          operand: anyPremise.operand,
          op: "eq",
          value,
          cOrder: anyPremise.cOrder,
        };
        if (!this.atomImplies(premise, p)) return false;
      }
      return any;
    }
    if (p.kind === "or") {
      if (this.clauseImplies(w, p.lhs) || this.clauseImplies(w, p.rhs)) return true;
    }
    const premise = comparisonAtom(w);
    return premise !== null && this.atomImplies(premise, p);
  }

  // Procedure 3 — a premise atom implies p.
  private atomImplies(premise: Premise, p: RExpr): boolean {
    if (!this.tick()) return false;
    if (p.kind === "and")
      return this.atomImplies(premise, p.lhs) && this.atomImplies(premise, p.rhs);
    if (p.kind === "or")
      return this.atomImplies(premise, p.lhs) || this.atomImplies(premise, p.rhs);
    // A TRUE comparison with a non-NULL scalar constant has a non-NULL operand.
    if (p.kind === "isNull" && p.negated) {
      return rexprEqShifted(premise.operand, p.operand, this.offset);
    }
    const target = comparisonAtom(p);
    if (target === null) return false;
    if (!rexprEqShifted(premise.operand, target.operand, this.offset)) return false;
    const ordering = (op: CmpOp): boolean => op !== "eq" && op !== "ne";
    if ((ordering(premise.op) || ordering(target.op)) && !(premise.cOrder && target.cOrder)) {
      return false;
    }
    const ord = constCmp(premise.value, target.value);
    return ord !== null && comparisonImplies(premise.op, target.op, ord);
  }
}
