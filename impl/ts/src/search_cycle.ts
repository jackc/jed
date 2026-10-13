// A recursive CTE's SEARCH / CYCLE clauses (spec/design/recursive-cte.md §9).
//
// The clauses add columns to the CTE: SEARCH an ordering column, CYCLE a cycle mark and a path.
// PostgreSQL defines them by a rewrite of the CTE (rewriteSearchCycle.c); jed computes the same
// columns from each term's produced rows. The planner resolves two lists of expressions over a
// synthetic row — one over an anchor row (the user columns), one over a recursive-term row (the user
// columns, then the parent row's added columns, which the recursive term carries as hidden trailing
// outputs) — plus CYCLE's filter, and the executor evaluates them per row.

import type { BinaryOp, Cte, CycleClause, Expr, QueryExpr, SrcLoc, TableRef } from "./ast.ts";
import type { Table } from "./catalog.ts";
import { engineError } from "./errors.ts";
import { evalExpr } from "./eval.ts";
import { commonType, requireSetopEquality } from "./eval_ops.ts";
import {
  type AggCtx,
  type CteBinding,
  type Engine,
  type EvalEnv,
  elemScalarHint,
  type ResolvedType,
  type RExpr,
  resolvedTypeEqual,
  type ScopeRel,
} from "./executor.ts";
import { resolve } from "./resolve.ts";
import { cteSyntheticTableCols, type ParamTypes, rtName, Scope } from "./scope.ts";
import type { Row } from "./storage.ts";
import { canonicalName } from "./types.ts";
import type { Value } from "./value.ts";

// SearchCyclePlan is the planned SEARCH / CYCLE columns of a recursive CTE (recursive-cte.md §9).
// Every expression is resolved against a synthetic row: anchor over an anchor row's userCols values;
// filter and recursive over a recursive-term row — its userCols values (coerced to the anchor's
// types) followed by the parent row's added columns.
export type SearchCyclePlan = {
  // userCols is the CTE's own column count; the added columns follow them.
  userCols: number;
  // names / types are the added columns', in order: the search column, then the cycle mark and the
  // cycle path.
  names: string[];
  types: ResolvedType[];
  // anchor gives the added columns' values for an anchor row, recursive for a recursive-term row.
  anchor: RExpr[];
  recursive: RExpr[];
  // filter is CYCLE's `mark <> value` over the parent's mark: a recursive-term row is kept only when
  // it is TRUE, so a row whose parent closed a cycle is not expanded. null without CYCLE.
  filter: RExpr | null;
};

// CycleMark is a CYCLE clause's mark constants, typed (recursive-cte.md §9): value marks a row that
// closes a cycle, def every other row. Both have the common type ty.
export type CycleMark = { value: Expr; def: Expr; ty: ResolvedType; defTy: ResolvedType };

function forbidden(): AggCtx {
  return { collecting: false, groupKeys: [], specs: [] };
}

// userCol is the synthetic column of user column i in an augment row. Never visible: the augment
// expressions build anonymous rows, whose fields are f1…fN whatever the columns are called.
function userCol(i: number): Expr {
  return { kind: "column", name: `*${i}*` };
}

// addedCol is the synthetic column of added column k (search, mark, path) in a recursive row.
function addedCol(k: number): Expr {
  return { kind: "column", name: `*added${k}*` };
}

function rowOf(cols: number[]): Expr {
  return { kind: "row", fields: cols.map(userCol) };
}

function intLit(n: bigint): Expr {
  return { kind: "literal", literal: { kind: "int", int: n } };
}

function binary(op: BinaryOp, lhs: Expr, rhs: Expr): Expr {
  return { kind: "binary", op, lhs, rhs };
}

function arrayOf(e: Expr): Expr {
  return { kind: "array", elements: [e] };
}

// resolveCycleMark types a CYCLE clause's mark constants (recursive-cte.md §9), before the CTE's body
// is planned (PostgreSQL's order). Omitted, they are TRUE / FALSE. A bare string literal takes the
// other constant's type, read through that type's input (`TO 1 DEFAULT 'N'` is 22P02, as in
// PostgreSQL); the two must then have a common type (42804), and that type an equality operator
// (42883).
export function resolveCycleMark(exec: Engine, c: CycleClause, ptypes: ParamTypes): CycleMark {
  let value: Expr;
  let def: Expr;
  if (c.markValue !== null && c.markDefault !== null) {
    value = c.markValue;
    def = c.markDefault;
  } else {
    value = { kind: "literal", literal: { kind: "bool", value: true } };
    def = { kind: "literal", literal: { kind: "bool", value: false } };
  }
  const scope = Scope.single(exec, cteSyntheticTableCols("*cycle*", [], [], null, null, null));
  const typeOf = (e: Expr): ResolvedType => resolve(scope, e, null, forbidden(), ptypes).type;
  let vt = typeOf(value);
  let dt = typeOf(def);
  // A bare string literal is PostgreSQL's unknown: it takes the other constant's type.
  const adapt = (e: Expr, other: ResolvedType): Expr => {
    if (e.kind !== "literal" || e.literal.kind !== "text") return e;
    const st = elemScalarHint(other);
    if (st === null || st === "text") return e;
    return { kind: "typedLiteral", typeName: canonicalName(st), text: e.literal.text, loc: e.loc };
  };
  value = adapt(value, dt);
  def = adapt(def, vt);
  vt = typeOf(value);
  dt = typeOf(def);
  let ty: ResolvedType;
  if (vt.kind === "null" && dt.kind === "null") ty = { kind: "text" };
  else if (vt.kind === "null") ty = dt;
  else if (dt.kind === "null") ty = vt;
  else {
    const t = commonType(vt, dt);
    if (t === null) {
      throw engineError(
        "datatype_mismatch",
        `CYCLE types ${rtName(vt)} and ${rtName(dt)} cannot be matched`,
      ).withPosition(c.loc);
    }
    ty = t;
  }
  requireSetopEquality(ty);
  return { value, def, ty, defTy: dt };
}

// planSearchCycle validates a recursive CTE's SEARCH / CYCLE clauses against its columns and resolves
// the added columns' expressions (recursive-cte.md §9). user is the CTE's relation (its columns named
// as the CTE names them) and userTypes their resolved types.
export function planSearchCycle(
  exec: Engine,
  cte: Cte,
  user: Table,
  userTypes: ResolvedType[],
  mark: CycleMark | null,
  ptypes: ParamTypes,
): SearchCyclePlan {
  const names = user.columns.map((c) => c.name);
  let searchCols: number[] = [];
  let cycleCols: number[] = [];
  const s = cte.search;
  const cy = cte.cycle;
  if (s !== null) {
    searchCols = clauseColumns(s.columns, names, "search", s.loc);
    if (names.includes(s.setColumn)) {
      throw clauseError(
        `search sequence column name "${s.setColumn}" already used in WITH query column list`,
        s.loc,
      );
    }
  }
  if (cy !== null) {
    cycleCols = clauseColumns(cy.columns, names, "cycle", cy.loc);
    for (const [col, what] of [
      [cy.markColumn, "mark"],
      [cy.pathColumn, "path"],
    ] as const) {
      if (names.includes(col)) {
        throw clauseError(
          `cycle ${what} column name "${col}" already used in WITH query column list`,
          cy.loc,
        );
      }
    }
    if (cy.markColumn === cy.pathColumn) {
      throw clauseError("cycle mark column name and cycle path column name are the same", cy.loc);
    }
  }
  if (s !== null && cy !== null) {
    for (const [col, what] of [
      [cy.markColumn, "mark"],
      [cy.pathColumn, "path"],
    ] as const) {
      if (s.setColumn === col) {
        throw clauseError(
          `search sequence column name and cycle ${what} column name are the same`,
          s.loc,
        );
      }
    }
  }

  // The added columns' expressions, over the synthetic anchor row (`*i*` = user column i) and
  // recursive row (then `*addedK*` = the parent's added column K).
  const addedNames: string[] = [];
  const anchorExprs: Expr[] = [];
  const recursiveExprs: Expr[] = [];
  let filter: Expr | null = null;
  if (s !== null) {
    const k = addedNames.length;
    addedNames.push(s.setColumn);
    if (s.breadthFirst) {
      // ROW(0, cols) / ROW(parent.f1 + 1, cols) — the depth, then the columns.
      const depth: Expr = { kind: "fieldAccess", base: addedCol(k), field: "f1" };
      anchorExprs.push({ kind: "row", fields: [intLit(0n), ...searchCols.map(userCol)] });
      recursiveExprs.push({
        kind: "row",
        fields: [binary("add", depth, intLit(1n)), ...searchCols.map(userCol)],
      });
    } else {
      // ARRAY[ROW(cols)] / parent || ARRAY[ROW(cols)] — the path of rows seen.
      anchorExprs.push(arrayOf(rowOf(searchCols)));
      recursiveExprs.push(binary("concat", addedCol(k), arrayOf(rowOf(searchCols))));
    }
  }
  if (cy !== null && mark !== null) {
    const markK = addedNames.length;
    addedNames.push(cy.markColumn);
    const pathK = addedNames.length;
    addedNames.push(cy.pathColumn);
    // The anchor's mark is the default, cast to the mark type when it widens.
    const widens = !resolvedTypeEqual(mark.defTy, mark.ty) && mark.defTy.kind !== "null";
    anchorExprs.push(
      widens
        ? { kind: "cast", inner: mark.def, typeName: rtName(mark.ty), typeMod: null }
        : mark.def,
    );
    anchorExprs.push(arrayOf(rowOf(cycleCols)));
    // CASE WHEN ROW(cols) = ANY (parent path) THEN value ELSE default END.
    recursiveExprs.push({
      kind: "case",
      operand: null,
      whens: [
        {
          cond: {
            kind: "quantified",
            op: "eq",
            all: false,
            lhs: rowOf(cycleCols),
            array: addedCol(pathK),
          },
          result: mark.value,
        },
      ],
      els: mark.def,
    });
    recursiveExprs.push(binary("concat", addedCol(pathK), arrayOf(rowOf(cycleCols))));
    filter = binary("ne", addedCol(markK), mark.value);
  }

  // Resolve over the anchor row; its types are the added columns' types.
  const userNames = userTypes.map((_, i) => `*${i}*`);
  const anchorRow = cteSyntheticTableCols("*anchor*", userNames, userTypes, null, null, null);
  const anchorScope = Scope.single(exec, anchorRow);
  const anchor: RExpr[] = [];
  const types: ResolvedType[] = [];
  for (const e of anchorExprs) {
    const r = resolve(anchorScope, e, null, forbidden(), ptypes);
    anchor.push(r.node);
    types.push(r.type);
  }
  // Resolve over the recursive row: the user columns, then the parent's added columns.
  const rowNames = [...userNames, ...types.map((_, k) => `*added${k}*`)];
  const recursiveRow = cteSyntheticTableCols(
    "*recursive*",
    rowNames,
    [...userTypes, ...types],
    null,
    null,
    null,
  );
  const recursiveScope = Scope.single(exec, recursiveRow);
  const recursive = recursiveExprs.map(
    (e) => resolve(recursiveScope, e, null, forbidden(), ptypes).node,
  );
  const filterNode =
    filter === null ? null : resolve(recursiveScope, filter, null, forbidden(), ptypes).node;
  return {
    userCols: userTypes.length,
    names: addedNames,
    types,
    anchor,
    recursive,
    filter: filterNode,
  };
}

// searchCycleRows gives each row its SEARCH / CYCLE columns (recursive-cte.md §9): it evaluates exprs
// over the row and replaces everything past the CTE's own columns with the results. With a filter, a
// row is dropped unless the filter is TRUE. Each row is a cost-ceiling check point; the expressions
// charge their ordinary evaluation cost, returned as cost.
export function searchCycleRows(
  exec: Engine,
  env: EvalEnv,
  plan: SearchCyclePlan,
  exprs: RExpr[],
  filter: RExpr | null,
  rows: Row[],
): { rows: Row[]; cost: bigint } {
  const meter = exec.session.newMeter();
  const acct = exec.session.queryAccount;
  const before = acct.measureRows(rows);
  const out: Row[] = [];
  for (const row of rows) {
    meter.guard();
    if (filter !== null) {
      const v: Value = evalExpr(filter, row, env, meter);
      if (v.kind !== "bool" || !v.value) continue;
    }
    const added = exprs.map((e) => evalExpr(e, row, env, meter));
    out.push([...row.slice(0, plan.userCols), ...added]);
  }
  const after = acct.measureRows(out);
  if (after >= before) acct.reserve(after - before);
  else acct.release(before - after);
  return { rows: out, cost: meter.accrued };
}

// starHiddenCount is how many trailing columns of rel `*` skips — a recursive CTE's SEARCH / CYCLE
// columns while its own recursive term is planned (recursive-cte.md §9).
export function starHiddenCount(rel: ScopeRel, ctes: CteBinding[]): number {
  return rel.cte === undefined ? 0 : (ctes[rel.cte]!.starHidden ?? 0);
}

// withCarriedColumns returns the recursive term with the self-reference's added columns appended as
// hidden trailing outputs (`self.seq`, `self.mark`, `self.path` — recursive-cte.md §9). The
// self-reference is a direct FROM / JOIN relation of the term's SELECT (analyzeRecursiveCte verified
// it); its label is its alias or the CTE's name.
export function withCarriedColumns(term: QueryExpr, cteName: string, added: string[]): QueryExpr {
  if (term.kind !== "select") throw new Error("a SEARCH / CYCLE recursive term is a SELECT");
  const refs: TableRef[] = [];
  if (term.from !== null) refs.push(term.from);
  for (const j of term.joins) refs.push(j.table);
  const self = refs.find(
    (t) =>
      t.subquery === undefined && t.args === null && t.values === undefined && t.name === cteName,
  );
  if (self === undefined) throw new Error("the recursive term references the CTE in its FROM");
  const label = self.alias ?? self.name;
  return {
    ...term,
    tail: added.map((name) => ({ kind: "qualifiedColumn", qualifier: label, name })),
  };
}

// notRecursiveError is SEARCH / CYCLE on a CTE that does not reference itself (recursive-cte.md §9),
// reported after its body is planned, as in PostgreSQL.
export function notRecursiveError(cte: Cte) {
  return engineError("syntax_error", "WITH query is not recursive").withPosition(
    cte.search?.loc ?? cte.cycle?.loc,
  );
}

// clauseColumns returns the CTE column positions a SEARCH / CYCLE column list names
// (recursive-cte.md §9): each must be a column of the CTE (42601) and appear once (42701). A name the
// CTE has twice takes its first column, as in PostgreSQL.
function clauseColumns(cols: string[], names: string[], what: string, loc?: SrcLoc): number[] {
  return cols.map((col, i) => {
    const at = names.indexOf(col);
    if (at < 0) throw clauseError(`${what} column "${col}" not in WITH query column list`, loc);
    if (cols.slice(0, i).includes(col)) {
      throw engineError(
        "duplicate_column",
        `${what} column "${col}" specified more than once`,
      ).withPosition(loc);
    }
    return at;
  });
}

function clauseError(message: string, loc?: SrcLoc) {
  return engineError("syntax_error", message).withPosition(loc);
}
