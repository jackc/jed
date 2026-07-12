// resolveProjections resolves SELECT items into evaluable projections (any result type is
// allowed in the select list, including boolean — SELECT a = b), each paired with its output
// column name (spec/design/grammar.md §8). `*` expands across ALL relations in FROM order,
// each relation's columns in catalog order (§15).
import {
  type Engine,
  type ParamTypes,
  type Scope,
  cmpBytes,
  collectColumn,
  elemScalarHint,
  isAggregateName,
  isHypotheticalSetName,
  isOrderedSetAggregateName,
  isWindowOnlyName,
  matchGroupExpr,
  matchPoly,
  noAggOverload,
  noFuncOverload,
  polyResultType,
  rangeBoundAssignable,
  resolveFuncCall,
  resolveHypotheticalSetAggregate,
  resolveOrderedSetAggregate,
  resolveWindowCall,
  resolvedRangeElementScalar,
  resolvedToScalar,
  resolvedTypeEqual,
  resolvedTypeOf,
  resolvedTypeOfCol,
  rtName,
  scalarPairCastable,
  undefinedColumn,
  valueToRExpr,
} from "./executor.ts";
import type {
  BinaryOp,
  Expr,
  JsonOnBehavior,
  JsonPredicateKind,
  JsonWrapper,
  QueryExpr,
  SelectItems,
} from "./ast.ts";
import type {
  AggCtx,
  ArrayFuncName,
  DeleteKind,
  HasKeyKind,
  JsonGetOp,
  JsonPathFnKind,
  JsonSqlKind,
  QueryPlan,
  RExpr,
  RSubscript,
  RangeOpName,
  RangeSetOpName,
  Resolved,
  ResolvedType,
} from "./executor.ts";
import { type EngineError, engineError } from "./errors.ts";
import { exprEqual, resolveTypeAndTypmod, scalarForParamHint, unifyCaseTypes } from "./eval_ops.ts";
import { coerceStringToArray, overflow, typeError } from "./store.ts";
import type { ScalarType } from "./types.ts";
import {
  classifyComparable,
  coerceStringLiteral,
  coerceStringToComposite,
  coerceStringToRangeExpr,
  ctxOf,
  dateArithResult,
  decodeByteaLiteral,
  decodeUuidLiteral,
  floatFromDecimalLiteral,
  intervalScaleResult,
  isAdaptableOperand,
  promote,
  requireBool,
  requireNumericOperand,
  requireTextOrNull,
  resolveOperandPair,
  temporalArithResult,
  unifyArrayElementTypes,
  widenFloatTo,
} from "./kernels.ts";
import { elementScalar, rangeByName } from "./range.ts";
import {
  canonicalName,
  compositeT,
  inRange,
  isBool,
  isBytea,
  isDate,
  isDecimal,
  isFloat,
  isInteger,
  isInterval,
  isJson,
  isJsonPath,
  isJsonb,
  isText,
  isTimestamp,
  isTimestamptz,
  isUuid,
  promoteFloat,
  rank,
  roundToWidth,
  scalarTypeFromName,
  typeIsText,
} from "./types.ts";
import { parseTimestamp, parseTimestamptz } from "./timestamp.ts";
import { dateClockSpecial, parseDate } from "./date.ts";
import { parseInterval } from "./interval.ts";
import { jsonCompactOut, jsonbIn, jsonbOut, parsePreservingJson, validateJson } from "./json.ts";
import {
  compile as jsonPathCompile,
  evalPath as jsonPathEval,
  render as jsonPathRender,
} from "./jsonpath.ts";
import type { ExtractSrc } from "./datetime_fn.ts";
import { extractField } from "./datetime_fn.ts";
import type { ColType, Column } from "./catalog.ts";
import type { RegexProgram } from "./regex.ts";
import { foldLowerSimple, loadedProperty, sortKey as collationSortKey } from "./collation.ts";
import { compileRegex } from "./regex.ts";
import type { Collation } from "./collation.ts";
import { OPERATORS } from "./operators.ts";
import type { Value } from "./value.ts";
import type { JsonNode, PathSetMode } from "./json.ts";
import { Decimal } from "./decimal.ts";
export function resolveProjections(
  scope: Scope,
  items: SelectItems,
  ag: AggCtx,
  params: ParamTypes,
): { nodes: RExpr[]; names: string[]; types: ResolvedType[] } {
  if (items.kind === "all") {
    // `*` with nothing to expand — a FROM-less SELECT — is PostgreSQL's exact error
    // (grammar.md §34). Qualifier-only rels don't count: they are RETURNING's old/new
    // pseudo-relations, and that scope always also carries the real relation.
    if (scope.rels.every((r) => r.qualifierOnly)) {
      throw engineError("syntax_error", "SELECT * with no tables specified is not valid");
    }
    const nodes: RExpr[] = [];
    const names: string[] = [];
    const types: ResolvedType[] = [];
    // USING/NATURAL merged columns come FIRST, in join order (PostgreSQL — grammar.md §15):
    // `SELECT * FROM a JOIN b USING(k)` is `k, <a's other cols>, <b's other cols>`. Each merge emits
    // its surviving-side column; its underlying copies are in `hidden` and so are skipped by the
    // per-relation loop below (otherwise the plain `*` expansion).
    for (const m of scope.merges) {
      const c = scope.columnAt(m.index);
      nodes.push({ kind: "column", index: m.index });
      names.push(c.name);
      types.push(resolvedTypeOfCol(c.type, scope.catalog));
    }
    // The RETURNING old/new pseudo-relations are qualifier-only: `*` expands the real
    // relations' columns exactly as before (grammar.md §32).
    for (const r of scope.rels) {
      if (r.qualifierOnly) continue;
      r.table.columns.forEach((c, i) => {
        const idx = r.offset + i;
        if (scope.hidden.includes(idx)) return;
        nodes.push({ kind: "column", index: idx });
        names.push(c.name);
        types.push(resolvedTypeOfCol(c.type, scope.catalog));
      });
    }
    return { nodes, names, types };
  }
  const nodes: RExpr[] = [];
  const names: string[] = [];
  const types: ResolvedType[] = [];
  for (const it of items.items) {
    // `t.*` expands the FROM relation labeled `qualifier` into one output column per column, in
    // catalog order (grammar.md §15) — like bare `*` but for one named relation and mixable with
    // other items. Resolved against the LOCAL scope only (like bare `*`); an unknown label is 42P01,
    // exactly as a qualified column ref.
    if (it.expr.kind === "qualifiedStar") {
      const want = it.expr.qualifier.toLowerCase();
      const rel = scope.rels.find((r) => r.label === want);
      if (rel === undefined) {
        throw engineError(
          "undefined_table",
          "missing FROM-clause entry for table " + it.expr.qualifier,
        );
      }
      rel.table.columns.forEach((c, i) => {
        nodes.push({ kind: "column", index: rel.offset + i });
        names.push(c.name);
        types.push(resolvedTypeOfCol(c.type, scope.catalog));
      });
      continue;
    }
    // `(expr).*` expands a composite base into one output column per field, in declaration order
    // (spec/design/composite.md §S4). The base AST is re-resolved per field (Expr is plain data,
    // resolution is pure) — deterministic. A non-composite base is 42809.
    if (it.expr.kind === "fieldStar") {
      const base = it.expr.base;
      const { type: baseType } = resolve(scope, base, null, ag, params);
      if (baseType.kind !== "composite") {
        throw engineError(
          "wrong_object_type",
          "column notation .* applied to type " +
            rtName(baseType) +
            ", which is not a composite type",
        );
      }
      baseType.fields.forEach((f, i) => {
        const { node: bn } = resolve(scope, base, null, ag, params);
        nodes.push({ kind: "field", base: bn, index: i });
        names.push(f.name);
        types.push(f.type);
      });
      continue;
    }
    const { node, type } = resolve(scope, it.expr, null, ag, params);
    nodes.push(node);
    types.push(type);
    names.push(it.alias ?? outputName(scope, it.expr));
  }
  return { nodes, names, types };
}

// outputName is the output column name of an un-aliased select item (grammar.md §8/§15): a
// bare or qualified column reference takes the catalog's canonical name (never the qualifier,
// never the SELECT spelling); every other expression takes the fixed "?column?". The column is
// known to exist — resolve validated it.
export function outputName(scope: Scope, e: Expr): string {
  // A bare/qualified column takes the catalog's canonical name, whether it resolves to a local
  // relation or (correlated) an enclosing one — columnOf handles both. A qualifier that names no
  // relation (the column.field ambiguity fallback) takes the written name (PG; matching Rust).
  if (e.kind === "column") {
    try {
      return scope.columnOf(scope.resolveBare(e.name)).name;
    } catch {
      return e.name;
    }
  }
  if (e.kind === "qualifiedColumn") {
    try {
      return scope.columnOf(scope.resolveQualified(e.qualifier, e.name)).name;
    } catch {
      return e.name;
    }
  }
  // An un-aliased aggregate call is named by its lowercased function name (PG; §8). A field
  // selection takes the FIELD name lowercased (PG names the output column after the field).
  if (e.kind === "funcCall") return e.name.toLowerCase();
  // The fixed keyword lowercased (PG; grammar.md §51) — no expression printer needed.
  if (e.kind === "coalesce") return "coalesce";
  // The fixed keyword lowercased (PG; grammar.md §52).
  if (e.kind === "greatestLeast") return e.greatest ? "greatest" : "least";
  if (e.kind === "fieldAccess") return e.field.toLowerCase();
  // A subscript takes the base array's name (PG names `a[1]` after `a`); `a[1][2]` recurses to the
  // same base. A non-column base falls through to `?column?`.
  if (e.kind === "subscript") return outputName(scope, e.base);
  return "?column?";
}

// orderAliasMatch resolves a bare ORDER BY name against the SELECT output columns — PostgreSQL's
// SQL92 rule that an ORDER BY simple name binds an OUTPUT column (an AS alias or an item's derived
// name — grammar.md §8/§10) BEFORE an input column, the opposite of GROUP BY's precedence. Returns
// the matching select-list item's expression (the caller routes it exactly like the same ordinal: a
// plain column stays on the slot fast path, a computed item is materialized), or null when no output
// name matches (the caller falls back to the FROM scope, the prior behavior). Matching is
// case-insensitive (§8). Only an explicit list is scanned — with * the output names are the scope
// columns, so the FROM-scope fallback already binds the same column. Two items of the same name with
// DIFFERENT expressions are ambiguous (42702); the same expression twice is not, matching PG.
export function orderAliasMatch(items: SelectItems, name: string, scope: Scope): Expr | null {
  if (items.kind !== "list") return null;
  const lower = name.toLowerCase();
  let found: Expr | null = null;
  for (const it of items.items) {
    const oname = it.alias ?? outputName(scope, it.expr);
    if (oname.toLowerCase() !== lower) continue;
    if (found === null) found = it.expr;
    else if (!exprEqual(found, it.expr))
      throw engineError("ambiguous_column", `ORDER BY "${name}" is ambiguous`);
  }
  return found;
}

// resolveBooleanFilter resolves a WHERE / ON expression; it must resolve to boolean (or an
// untyped NULL, always unknown → no rows). An integer- or text-valued one is a 42804.
export function resolveBooleanFilter(scope: Scope, e: Expr, params: ParamTypes): RExpr {
  // WHERE / ON filters run before any grouping, so an aggregate here is 42803 (Forbidden).
  const { node, type } = resolve(
    scope,
    e,
    null,
    { collecting: false, groupKeys: [], specs: [] },
    params,
  );
  if (type.kind !== "bool" && type.kind !== "null") {
    throw typeError("argument of WHERE must be boolean");
  }
  return node;
}

// resolveColumnRef turns a chain resolution into a resolved node + type (§26). A Local column
// obeys the grouping rule (collectColumn); an Outer (correlated) reference is a per-outer-row
// CONSTANT, so it bypasses that rule and resolves to an outerColumn reading the enclosing row at
// eval; its type is the ancestor column's.
export function resolveColumnRef(
  scope: Scope,
  ag: AggCtx,
  r: Resolved,
  name: string,
): { node: RExpr; type: ResolvedType } {
  if (r.level === 0) return collectColumn(scope, ag, r.index, name);
  return {
    node: { kind: "outerColumn", level: r.level, index: r.index },
    type: resolvedTypeOfCol(scope.columnOf(r).type, scope.catalog),
  };
}

// resolveFieldOf resolves a composite field selection `base.field` (spec/design/composite.md §S4)
// given the already-resolved `base` node and its static type: `base` must be composite — else 42809
// (wrong_object_type, PG's "column notation applied to non-composite") — and `field` must name one
// of its fields case-insensitively (PG folds the identifier), else 42703 (undefined_column). Returns
// the `field` RExpr node carrying the fixed field ordinal, plus the field's static type.
export function resolveFieldOf(
  baseNode: RExpr,
  baseType: ResolvedType,
  field: string,
): { node: RExpr; type: ResolvedType } {
  if (baseType.kind !== "composite") {
    throw engineError(
      "wrong_object_type",
      "column notation ." +
        field +
        " applied to type " +
        rtName(baseType) +
        ", which is not a composite type",
    );
  }
  const lower = field.toLowerCase();
  const idx = baseType.fields.findIndex((f) => f.name.toLowerCase() === lower);
  if (idx < 0) throw undefinedColumn(field);
  return {
    node: { kind: "field", base: baseNode, index: idx },
    type: baseType.fields[idx]!.type,
  };
}

// planSubquery plans a subquery operand against the scope chain (§26). Rejects a non-SELECT context
// (UPDATE/DELETE/INSERT — allowSubquery false) with 0A000. A $N inside the subquery is allowed: the
// shared params table is threaded into the inner plan, so a parameter typed by an inner context
// (WHERE inner.col = $1) infers statement-wide and unifies with any outer use of the same $N. A
// parameter with NO type context anywhere stays uninferred and finalize raises 42P18 (a documented
// divergence from PostgreSQL, which defaults such a $N to text — grammar.md §26). The inner query is
// resolved ONCE, with `scope` as its parent, so correlated references become outerColumn and errors
// fire even over an empty outer.
export function planSubquery(scope: Scope, inner: QueryExpr, params: ParamTypes): QueryPlan {
  if (!scope.allowSubquery) {
    throw engineError(
      "feature_not_supported",
      "subqueries are only supported in a SELECT statement",
    );
  }
  // Any subquery makes the enclosing plan un-cacheable: the fold pass rewrites an uncorrelated one
  // (or an uncorrelated one nested inside a correlated one) into a constant using THIS execution's
  // bound params, so a reused plan would carry another execution's folded constants. Every subquery
  // form (scalar / EXISTS / IN / quantified) funnels through here.
  params.uncacheable = true;
  // The subquery inherits the enclosing scope's CTE bindings directly (cte.md §2) — visible at any
  // nesting depth without counting as a correlation level.
  return scope.catalog.planQuery(inner, scope, scope.ctes, params);
}

// dateClockLiteral resolves a date-context string literal naming one of the special values beyond
// ±infinity (date.md §6): 'epoch' folds to the constant 1970-01-01 like any date literal, while
// the CLOCK-RELATIVE words 'today' / 'now' / 'tomorrow' / 'yesterday' become the STABLE dateClock
// node — the statement clock's day in the session zone, computed at EVAL and never folded at
// resolve. (PostgreSQL folds the literal at parse — the frozen-'today'
// DEFAULT/index/prepared-statement footgun — a documented divergence; jed's node re-evaluates per
// execution, so a cached plan tracks the clock.) The node flags the plan non-immutable, exactly
// like the runtime text→date cast (42P17 in an index expression). null for an ordinary date
// string, which takes the caller's normal parse-to-constant path.
function dateClockLiteral(
  s: string,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } | null {
  const sp = dateClockSpecial(s);
  if (sp === null) return null;
  if (sp.epoch) return { node: { kind: "constDate", value: 0n }, type: { kind: "date" } };
  params.nonimmutable = true;
  return { node: { kind: "dateClock", offsetDays: sp.offsetDays }, type: { kind: "date" } };
}

// resolve resolves one Expr into an RExpr plus its static type. ctx (non-null) is the
// type an untyped integer literal should adapt to (spec/design/types.md §6); null
// defaults a bare literal to i64.
export function resolve(
  scope: Scope,
  e: Expr,
  ctx: ScalarType | null,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  // GROUP BY a general expression (aggregates.md §15): a non-column expression that structurally
  // matches a grouping-expression key resolves to that group's synthetic key slot — so `SELECT a+b
  // … GROUP BY a+b` projects the grouped value, like a grouping column. Columns keep their own path
  // (matched by index); an aggregate operand / FILTER resolves under the Forbidden mode (no
  // groupKeyExprs), so this is correctly inert there (its `a+b` is a per-row value, not the group key).
  if (e.kind !== "column" && e.kind !== "qualifiedColumn") {
    const m = matchGroupExpr(ag, e);
    if (m !== null) return { node: { kind: "column", index: m.slot }, type: m.ty };
  }
  switch (e.kind) {
    case "row": {
      // A ROW(...) constructor (spec/design/composite.md §1): resolve each field with no type
      // context (its natural type), producing an ANONYMOUS composite (name = null, fields named
      // f1, f2, … per PG). Storing it into a named composite column matches structurally
      // (assignability at the store site coerces each field to the target's declared type).
      const nodes: RExpr[] = [];
      const fields: { name: string; type: ResolvedType }[] = [];
      for (let i = 0; i < e.fields.length; i++) {
        const { node, type } = resolve(scope, e.fields[i]!, null, ag, params);
        nodes.push(node);
        fields.push({ name: "f" + (i + 1), type });
      }
      return {
        node: { kind: "row", fields: nodes },
        type: { kind: "composite", name: null, fields },
      };
    }
    case "array": {
      // An ARRAY[...] constructor (spec/design/array.md §1): resolve each element (natural type),
      // unify to a common element type, build an array node. A bare empty ARRAY[] has no element
      // type to infer — use '{}'::T[] instead (the cast supplies it).
      if (e.elements.length === 0) {
        throw typeError("cannot determine the element type of an empty ARRAY[]; write '{}'::T[]");
      }
      // An element-type hint (ctx) flows down to the elements so an array literal adapts its untyped
      // integer/decimal literals exactly as a scalar literal does — e.g. resolving ARRAY[7,8] with an
      // i32 context yields i32[], not the default i64[] (the polymorphic array functions pass the
      // bound element type here, array-functions.md §2). Almost every other caller passes null, so the
      // default 1-D unification is unchanged.
      const nodes: RExpr[] = [];
      const elemTypes: ResolvedType[] = [];
      for (const el of e.elements) {
        const { node, type } = resolve(scope, el, ctx, ag, params);
        nodes.push(node);
        elemTypes.push(type);
      }
      // If the items are themselves arrays, this is a nested (multidim-stacking) constructor and the
      // result type is the SAME array type (dimension-agnostic, §2/§4); otherwise a flat 1-D array.
      const common = unifyArrayElementTypes(elemTypes);
      if (common.kind === "array") {
        return {
          node: { kind: "array", elements: nodes, nested: true },
          type: common,
        };
      }
      return {
        node: { kind: "array", elements: nodes, nested: false },
        type: { kind: "array", elem: common },
      };
    }
    case "column": {
      // Resolve against the scope CHAIN (§26). A Local match obeys the grouping rule; an Outer
      // (correlated) match is a per-outer-row constant exempt from it (resolveColumnRef).
      return resolveColumnRef(scope, ag, scope.resolveBare(e.name), e.name);
    }
    case "qualifiedColumn": {
      // A bare `rel.col` resolves STRICTLY against the FROM relations — `qualifier` MUST name a
      // relation (else 42P01), matching PostgreSQL. Composite field access on a column is the
      // PARENS-REQUIRED `(col).field` form (spec/design/composite.md §1/§S4), a fieldAccess node,
      // never this bare qualified-column path (PG raises 42P01 for the unparenthesized `col.field` /
      // `t.col.field` spellings).
      return resolveColumnRef(scope, ag, scope.resolveQualified(e.qualifier, e.name), e.name);
    }
    case "fieldAccess": {
      // `(expr).field` — composite field selection (spec/design/composite.md §S4).
      const { node, type } = resolve(scope, e.base, null, ag, params);
      return resolveFieldOf(node, type, e.field);
    }
    case "fieldStar":
      // `(expr).*` — whole-row expansion is a projection-list construct only; in a scalar
      // expression position it is unsupported (PG rejects row expansion here — 0A000).
      throw engineError(
        "feature_not_supported",
        "row expansion (.*) is not supported in this context",
      );
    case "qualifiedStar":
      // `t.*` is likewise projection-list only — resolveProjections expands it before ever calling
      // resolve(); reaching here means it appeared in a scalar position (which the parser already
      // rejects as 42601). Defensive parity with the fieldStar arm.
      throw engineError("syntax_error", "t.* is only allowed in a select list");
    case "subscript": {
      // `base[..][..]` — array subscript (spec/design/array.md §6). The base must be an array (else
      // 42804). Each subscript bound is an integer (a literal adapts; a non-integer is 42804). If any
      // spec is a slice the result is the array type (a sub-array); otherwise the element type. OOB /
      // NULL → NULL is an evaluation-time rule, not a resolve error.
      const base = resolve(scope, e.base, null, ag, params);
      if (base.type.kind !== "array") {
        throw typeError(
          `cannot subscript a value of type ${rtName(base.type)}, which is not an array`,
        );
      }
      const resolveBound = (b: Expr): RExpr => {
        const r = resolve(scope, b, "i32", ag, params);
        if (r.type.kind !== "int" && r.type.kind !== "null") {
          throw typeError(`array subscript must be an integer, not ${rtName(r.type)}`);
        }
        return r.node;
      };
      let isSlice = false;
      const rsubs: RSubscript[] = e.subscripts.map((s) => {
        if (s.isSlice) {
          isSlice = true;
          return {
            isSlice: true,
            lower: s.lower === null ? null : resolveBound(s.lower),
            upper: s.upper === null ? null : resolveBound(s.upper),
          };
        }
        return { isSlice: false, index: resolveBound(s.index) };
      });
      // A slice yields a sub-array (the array type); all-index access yields an element.
      const type = isSlice ? base.type : base.type.elem;
      return {
        node: {
          kind: "subscript",
          base: base.node,
          subscripts: rsubs,
          isSlice,
        },
        type,
      };
    }
    case "param": {
      // A bind parameter is an adaptable operand (like an integer/string literal): it takes its
      // type from ctx — the sibling operand, target column, or CAST target. Record the inferred
      // type (null = no context here; finalize 42P18s a parameter that never gets one).
      const idx0 = e.index - 1;
      params.note(idx0, ctx);
      const type: ResolvedType = ctx !== null ? resolvedTypeOf(ctx) : { kind: "null" };
      return { node: { kind: "param", index: idx0 }, type };
    }
    case "funcCall": {
      // A hypothetical-set aggregate (rank/dense_rank/percent_rank/cume_dist — aggregates.md §19) is
      // one of these window-function names used WITH a WITHIN GROUP clause; that clause routes it here
      // instead of the window path. OVER + WITHIN GROUP together is 0A000.
      if (isHypotheticalSetName(e.name) && e.withinGroup !== undefined && e.withinGroup !== null) {
        if (
          (e.over !== undefined && e.over !== null) ||
          (e.overName !== undefined && e.overName !== null)
        ) {
          throw engineError(
            "feature_not_supported",
            `OVER is not supported for hypothetical-set aggregate ${e.name.toLowerCase()}`,
          );
        }
        return resolveHypotheticalSetAggregate(scope, e, ag, params);
      }
      // An ordered-set aggregate (mode/percentile_cont/percentile_disc — aggregates.md §13) carries
      // WITHIN GROUP and is resolved by its own path. OVER on one is 0A000 (PG itself does not support
      // an ordered-set aggregate as a window function); WITHOUT a WITHIN GROUP it is 42883 (PG:
      // "function mode() does not exist").
      if (isOrderedSetAggregateName(e.name)) {
        if (
          (e.over !== undefined && e.over !== null) ||
          (e.overName !== undefined && e.overName !== null)
        ) {
          throw engineError(
            "feature_not_supported",
            `OVER is not supported for ordered-set aggregate ${e.name.toLowerCase()}`,
          );
        }
        if (e.withinGroup === undefined || e.withinGroup === null) {
          throw noAggOverload(e.name.toLowerCase());
        }
        return resolveOrderedSetAggregate(scope, e, ag, params);
      }
      // WITHIN GROUP on a non-ordered-set function (an ordinary aggregate or a scalar function) is
      // 42883 — PG models it as a missing overload (`sum(numeric, numeric) does not exist`).
      if (e.withinGroup !== undefined && e.withinGroup !== null) {
        throw noAggOverload(e.name.toLowerCase());
      }
      // A trailing OVER makes this a window-function call (spec/design/window.md §5.1).
      if (e.over !== undefined && e.over !== null) {
        // GROUPING is not a window function — GROUPING(a) OVER () is a syntax error in PostgreSQL
        // (42601); match it rather than treating GROUPING as an unknown window function.
        if (e.name.toLowerCase() === "grouping") {
          throw engineError("syntax_error", "OVER is not supported for GROUPING");
        }
        // DISTINCT is not implemented for window functions (PG 0A000 — aggregates.md §5): a window
        // aggregate folds over a frame, where per-frame de-duplication is undefined.
        if (e.distinct) {
          throw engineError(
            "feature_not_supported",
            "DISTINCT is not implemented for window functions",
          );
        }
        // FILTER over a window function (aggregates.md §20). A window AGGREGATE folds only the frame
        // rows for which the filter is TRUE; a pure (non-aggregate) window function with FILTER is
        // PG's own 0A000 ("FILTER is not implemented for non-aggregate window functions"). The filter
        // is threaded into the WindowSpec and applied in the window stage.
        if (e.filter !== undefined && e.filter !== null && !isAggregateName(e.name)) {
          throw engineError(
            "feature_not_supported",
            "FILTER is not implemented for non-aggregate window functions",
          );
        }
        return resolveWindowCall(
          scope,
          { name: e.name, args: e.args, star: e.star, over: e.over, filter: e.filter },
          ag,
          params,
        );
      }
      // A window-only function (row_number/…) used WITHOUT OVER is 42809 (PG's wrong_object_type,
      // not the windowing_error 42P20 it uses for a window in WHERE — window.md §7, oracle-verified).
      if (isWindowOnlyName(e.name)) {
        throw engineError(
          "wrong_object_type",
          `window function ${e.name.toLowerCase()} requires an OVER clause`,
        );
      }
      return resolveFuncCall(scope, e, ag, params);
    }
    case "typedLiteral": {
      // A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's
      // `type 'string'`, equal to CAST('string' AS type) over a string-literal operand. Resolve the
      // type by name (unknown → 42704) and coerce the string to it at resolve, context-free. No
      // typmod rides on the literal (the parser's one-token lookahead admits none).
      // A composite type name (`addr '(Main,90210)'`) coerces the string via record_in
      // (spec/design/composite.md §8) — the same primitive as `'(…)'::addr`.
      const ct = scope.catalog.compositeType(e.typeName);
      if (ct !== undefined) return coerceStringToComposite(e.text, ct, scope.catalog);
      // A range type name (`i32range '[1,5)'`, `int4range '…'`) coerces the string via range_in
      // against the element type (spec/design/ranges.md §5) — the same primitive as the cast.
      const rdesc = rangeByName(e.typeName);
      if (rdesc !== undefined) return coerceStringToRangeExpr(e.text, rdesc);
      const [target] = resolveTypeAndTypmod(e.typeName, null);
      // DATE 'today' / DATE 'now' / … — the clock-relative specials become the STABLE dateClock
      // node, exactly like the ctx-adaptation form (date.md §6).
      if (isDate(target)) {
        const clock = dateClockLiteral(e.text, params);
        if (clock !== null) return clock;
      }
      return coerceStringLiteral(e.text, target, null, null);
    }
    case "literal":
      switch (e.literal.kind) {
        case "null":
          return { node: { kind: "constNull" }, type: { kind: "null" } };
        case "bool":
          return {
            node: { kind: "constBool", value: e.literal.value },
            type: { kind: "bool" },
          };
        case "text": {
          // A string literal is text by default (collation C). It adapts to a BYTEA or a UUID
          // context (types.md §6/§13/§14): decode the hex input (bytea) or the PG-flexible uuid
          // input (uuid) — 22P02 on malformed; any other context — including none — keeps it text.
          // A string literal is text by default (collation C). It adapts to a BYTEA context
          // (decode the hex input, 22P02 on bad hex) or a TIMESTAMP/TIMESTAMPTZ context (parse
          // the datetime, 22007/22008 — spec/design/timestamp.md). Any other context keeps it text.
          if (ctx !== null && isBytea(ctx)) {
            return {
              node: {
                kind: "constBytea",
                value: decodeByteaLiteral(e.literal.text),
              },
              type: { kind: "bytea" },
            };
          }
          if (ctx !== null && isUuid(ctx)) {
            return {
              node: {
                kind: "constUuid",
                value: decodeUuidLiteral(e.literal.text),
              },
              type: { kind: "uuid" },
            };
          }
          if (ctx !== null && isTimestamp(ctx)) {
            return {
              node: {
                kind: "constTimestamp",
                value: parseTimestamp(e.literal.text),
              },
              type: { kind: "timestamp" },
            };
          }
          if (ctx !== null && isTimestamptz(ctx)) {
            return {
              node: {
                kind: "constTimestamptz",
                value: parseTimestamptz(e.literal.text),
              },
              type: { kind: "timestamptz" },
            };
          }
          if (ctx !== null && isDate(ctx)) {
            // A string adapts to a DATE context (the ISO date, dropping any time/offset; date.md
            // §2). A clock-relative special ('today'/'now'/…) becomes the STABLE dateClock node
            // instead of a constant (date.md §6).
            const clock = dateClockLiteral(e.literal.text, params);
            if (clock !== null) return clock;
            return {
              node: { kind: "constDate", value: parseDate(e.literal.text) },
              type: { kind: "date" },
            };
          }
          if (ctx !== null && isInterval(ctx)) {
            // A string adapts to an INTERVAL context (parse the "unit + time" subset,
            // 22007/22008 — spec/design/interval.md), like timestamp adaptation.
            return {
              node: {
                kind: "constInterval",
                value: parseInterval(e.literal.text),
              },
              type: { kind: "interval" },
            };
          }
          if (ctx !== null && isJson(ctx)) {
            // A string adapts to a JSON context (validate, store verbatim — spec/design/json.md §4);
            // malformed → 22P02.
            validateJson(e.literal.text);
            return { node: { kind: "constJson", value: e.literal.text }, type: { kind: "json" } };
          }
          if (ctx !== null && isJsonb(ctx)) {
            // A string adapts to a JSONB context (parse + canonicalize — §2); malformed → 22P02.
            return {
              node: { kind: "constJsonb", value: jsonbIn(e.literal.text) },
              type: { kind: "jsonb" },
            };
          }
          if (ctx !== null && isJsonPath(ctx)) {
            // A string adapts to a jsonpath context (a jsonpath function argument) — it is compiled
            // to a path at resolve (jsonpath.md §1); malformed → 42601 / unsupported → 0A000.
            return {
              node: {
                kind: "constJsonPath",
                value: jsonPathRender(jsonPathCompile(e.literal.text)),
              },
              type: { kind: "jsonpath" },
            };
          }
          return {
            node: { kind: "constText", value: e.literal.text },
            type: { kind: "text" },
          };
        }
        case "decimal":
          // A decimal literal adapts to a FLOAT context (float.md §4): decimal → float at resolve
          // (the nearest binary64 to the exact decimal; Math.fround if the context is f32). The
          // exact-decimal string already round-trips IEEE conversion via Number(...). Otherwise it
          // is decimal — cap-checked here (an over-long coefficient/scale traps 22003 at resolve).
          if (ctx !== null && isFloat(ctx)) {
            return floatFromDecimalLiteral(e.literal.dec, ctx);
          }
          return {
            node: { kind: "constDecimal", value: e.literal.dec.checkCap() },
            type: { kind: "decimal" },
          };
        default: {
          // An integer literal adapts to an integer context or — like a decimal literal — a FLOAT
          // context (int → float at resolve; float.md §4). A non-numeric context (a text/decimal
          // column or assignment target) does not apply — it defaults to i64, and the surrounding
          // check then reports the family mismatch (42804) or widens it (int→decimal), never a wrong
          // range check.
          if (ctx !== null && isFloat(ctx)) {
            const n = roundToWidth(ctx, Number(e.literal.int));
            return {
              node: { kind: "constFloat", ty: ctx, value: n },
              type: { kind: "float", ty: ctx },
            };
          }
          const ty = ctx !== null && isInteger(ctx) ? ctx : "i64";
          if (!inRange(ty, e.literal.int)) throw overflow(ty);
          return {
            node: { kind: "constInt", value: e.literal.int },
            type: { kind: "int", ty },
          };
        }
      }
    case "scalarSubquery": {
      // A subquery in expression position (§26): PLANNED ONCE against the scope chain here, so its
      // column-count / type errors fire even over an empty outer. planSubquery rejects a non-SELECT
      // context and a $N inside (both 0A000). The fold pass folds an uncorrelated one to a constant;
      // a correlated one is re-executed per outer row by the evaluator.
      const plan = planSubquery(scope, e.query, params);
      if (plan.columnTypes.length !== 1) {
        throw engineError("syntax_error", "subquery must return only one column");
      }
      return {
        node: {
          kind: "subquery",
          plan,
          subKind: "scalar",
          lhs: null,
          negated: false,
        },
        type: plan.columnTypes[0]!,
      };
    }
    case "exists": {
      // EXISTS ignores the select list entirely; the result is boolean, never NULL. A NOT EXISTS
      // parses as the unary NOT wrapping this, so negated here is always false.
      const plan = planSubquery(scope, e.query, params);
      return {
        node: {
          kind: "subquery",
          plan,
          subKind: "exists",
          lhs: null,
          negated: false,
        },
        type: { kind: "bool" },
      };
    }
    case "inSubquery": {
      // The LHS is an OUTER expression (resolved in the current scope / agg context); the subquery
      // yields the single membership column. The test is `lhs = element`, so the pair must be
      // comparable (42804), exactly like a literal IN.
      const { node: lhs, type: lt } = resolve(scope, e.lhs, null, ag, params);
      const plan = planSubquery(scope, e.query, params);
      if (plan.columnTypes.length !== 1) {
        throw engineError("syntax_error", "subquery has too many columns");
      }
      classifyComparable(lt, plan.columnTypes[0]!);
      return {
        node: {
          kind: "subquery",
          plan,
          subKind: "in",
          lhs,
          negated: e.negated,
        },
        type: { kind: "bool" },
      };
    }
    case "quantifiedSubquery": {
      // The subquery spelling of the quantifier (array-functions.md §11.6) — the IN-subquery
      // pattern with the comparison + 3VL fold. Resolve the outer lhs, plan the body, require ONE
      // column (42601), and require comparability — reporting operator-not-found (42883) the way the
      // array quantifier does (§11.3), not the plain 42804. No 21000 cardinality limit.
      const { node: lhs, type: lt } = resolve(scope, e.lhs, null, ag, params);
      const plan = planSubquery(scope, e.query, params);
      if (plan.columnTypes.length !== 1) {
        throw engineError("syntax_error", "subquery has too many columns");
      }
      try {
        classifyComparable(lt, plan.columnTypes[0]!);
      } catch {
        throw engineError(
          "undefined_function",
          `operator does not exist: ${rtName(lt)} ${binaryOpSymbol(e.op)} ${rtName(plan.columnTypes[0]!)}`,
        );
      }
      return {
        node: {
          kind: "subquery",
          plan,
          subKind: "quantified",
          lhs,
          negated: false,
          op: e.op,
          all: e.all,
        },
        type: { kind: "bool" },
      };
    }
    case "collate": {
      // `expr COLLATE "name"` (spec/design/collation.md §1) — a postfix collation operator. Resolve
      // the inner expression, require a collatable (text) type (42804, PG-matching), and validate the
      // named collation exists ("C" or loaded, else 42704). A runtime PASSTHROUGH: a collation only
      // changes the ORDERING comparisons / ORDER BY, derived from the AST at those sites
      // (explicitCollation / OrderKey.collation), so resolving returns the inner node + type
      // unchanged. The hint flows through (COLLATE never changes the type).
      const r = resolve(scope, e.inner, ctx, ag, params);
      if (r.type.kind !== "text" && r.type.kind !== "null") {
        throw typeError(`collations are not supported by type ${rtName(r.type)}`);
      }
      resolveCollationName(scope.catalog, e.collation); // surfaces 42704 for an unknown name
      return r;
    }
    case "extract": {
      // EXTRACT(field FROM source) (timezones.md §9.2, grammar.md §50). The field is SYNTACTIC and
      // validated at RESOLVE (not per row): an unsupported field for the source type is 0A000, an
      // unrecognized field is 22023 — surfaced by probing the kernel with a zero value of the source's
      // family. The source must be a datetime type (else 42883); the result is numeric.
      const src = resolve(scope, e.source, null, ag, params);
      // A NULL source has no resolvable family; the value propagates to NULL at eval (the field is
      // not validated — a documented narrow edge vs. PG).
      if (src.type.kind !== "null") {
        let probe: ExtractSrc;
        switch (src.type.kind) {
          case "timestamp":
            probe = { kind: "ts", micros: 0n };
            break;
          case "timestamptz":
            probe = { kind: "tstz", instant: 0n, local: 0n, offsetSecs: 0n };
            break;
          case "date":
            probe = { kind: "date", days: 0n };
            break;
          case "interval":
            probe = { kind: "interval", iv: { months: 0, days: 0, micros: 0n } };
            break;
          default:
            throw engineError(
              "undefined_function",
              `function extract(text, ${rtName(src.type)}) does not exist`,
            );
        }
        extractField(e.field, probe); // validate field-for-type (0A000 / 22023); value discarded
      }
      return {
        node: { kind: "extract", field: e.field, value: src.node },
        type: { kind: "decimal" },
      };
    }
    case "cast": {
      // An array cast target `…::T[]` (spec/design/array.md §7). v1 supports only the string-literal
      // form `'{…}'::T[]` and a bare NULL; every other array cast (runtime text→array, array→text,
      // element-wise array→array) is a documented 0A000 narrowing.
      if (e.typeName.endsWith("[]")) {
        const base = e.typeName.slice(0, -2);
        if (e.typeMod !== null) {
          throw engineError(
            "feature_not_supported",
            "a type modifier on an array type is not supported yet",
          );
        }
        const elemScalar = scalarTypeFromName(base);
        const baseComposite = scope.catalog.compositeType(base);
        let elemCol: ColType;
        let elemRt: ResolvedType;
        if (elemScalar !== undefined) {
          elemCol = { kind: "scalar", scalar: elemScalar };
          elemRt = resolvedTypeOf(elemScalar);
        } else if (baseComposite !== undefined) {
          const elemTy = compositeT(baseComposite.name);
          elemCol = scope.catalog.colTypeOf(elemTy);
          elemRt = resolvedTypeOfCol(elemTy, scope.catalog);
        } else {
          throw engineError("undefined_object", "type does not exist: " + base);
        }
        if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
          const val = coerceStringToArray(e.inner.literal.text, elemCol);
          return {
            node: valueToRExpr(val),
            type: { kind: "array", elem: elemRt },
          };
        }
        if (e.inner.kind === "literal" && e.inner.literal.kind === "null") {
          return {
            node: { kind: "constNull" },
            type: { kind: "array", elem: elemRt },
          };
        }
        // A bind parameter into an array stays the container-param narrowing (0A000), like INSERT's
        // $N-into-a-container handling (spec/design/array.md §4).
        if (e.inner.kind === "param") {
          throw engineError(
            "feature_not_supported",
            "casting a parameter to an array type is not supported yet",
          );
        }
        // A runtime (non-literal) operand: the two follow-on array-producing casts (array.md §7).
        // A text expression coerces per row via array_in (runtime text→T[]); an array of the SAME
        // element type is the identity (no node); an array of a DIFFERENT element type is an
        // element-wise array→array cast (each element through the scalar cast, when the element pair
        // is castable); a non-literal NULL adapts. Any other source is a 42804.
        const inner = resolve(scope, e.inner, null, ag, params);
        const resultType: ResolvedType = { kind: "array", elem: elemRt };
        if (inner.type.kind === "null") {
          return { node: inner.node, type: resultType };
        }
        if (inner.type.kind === "text") {
          return {
            node: { kind: "arrayCast", toElem: elemCol, operand: inner.node },
            type: resultType,
          };
        }
        if (inner.type.kind === "array") {
          if (resolvedTypeEqual(inner.type.elem, elemRt)) {
            return { node: inner.node, type: resultType }; // identity cast — same element type
          }
          const srcS = resolvedToScalar(inner.type.elem);
          if (
            srcS !== null &&
            elemCol.kind === "scalar" &&
            scalarPairCastable(srcS, elemCol.scalar)
          ) {
            return {
              node: { kind: "arrayCast", toElem: elemCol, operand: inner.node },
              type: resultType,
            };
          }
          // A composite element on either side is the deferred composite cast surface (0A000).
          if (srcS === null || elemCol.kind === "composite") {
            throw engineError(
              "feature_not_supported",
              "casting between composite-element arrays is not supported yet",
            );
          }
          // Both elements are scalars but no cast exists between them — forbidden (42804; jed's
          // strict-matrix convention, PG reports 42846).
          throw engineError(
            "datatype_mismatch",
            "cannot cast " + rtName(inner.type) + " to " + base + "[]",
          );
        }
        throw engineError(
          "datatype_mismatch",
          "cannot cast " + rtName(inner.type) + " to " + base + "[]",
        );
      }
      // A range cast target (`'[1,5)'::i32range`, `…::int4range`). Like array, v1 supports the
      // string-literal form and a bare NULL; every other range cast is a 0A000 narrowing
      // (spec/design/ranges.md §1/§5).
      {
        const rdesc = rangeByName(e.typeName);
        if (rdesc !== undefined) {
          if (e.typeMod !== null) {
            throw engineError(
              "feature_not_supported",
              "a type modifier on a range type is not supported",
            );
          }
          const elemRt = resolvedTypeOf(elementScalar(rdesc));
          if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
            return coerceStringToRangeExpr(e.inner.literal.text, rdesc);
          }
          if (e.inner.kind === "literal" && e.inner.literal.kind === "null") {
            return {
              node: { kind: "constNull" },
              type: { kind: "range", elem: elemRt },
            };
          }
          throw engineError(
            "feature_not_supported",
            "casting to a range type is only supported from a string literal this slice",
          );
        }
      }
      // A composite cast target (`'(…)'::addr`) — a CREATE TYPE name, not a built-in scalar
      // (spec/design/composite.md §8). A STRING LITERAL operand coerces via record_in (the
      // `'(…)'::addr` headline); a bare NULL adapts to the composite; a same-named composite operand
      // is the identity. Every other operand (a runtime text expression, an anonymous `ROW(…)`) is a
      // documented 0A000 narrowing this slice — relaxable. A type modifier on a composite is
      // meaningless (0A000).
      const ct = scope.catalog.compositeType(e.typeName);
      if (ct !== undefined) {
        if (e.typeMod !== null) {
          throw engineError(
            "feature_not_supported",
            "a type modifier is not supported on a composite type",
          );
        }
        if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
          return coerceStringToComposite(e.inner.literal.text, ct, scope.catalog);
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        if (inner.type.kind === "null") {
          return {
            node: inner.node,
            type: resolvedTypeOfCol({ kind: "composite", name: ct.name }, scope.catalog),
          };
        }
        // An identical named composite is the identity cast.
        if (
          inner.type.kind === "composite" &&
          inner.type.name?.toLowerCase() === ct.name.toLowerCase()
        ) {
          return inner;
        }
        throw engineError(
          "feature_not_supported",
          "casting to a composite type is only supported from a string literal",
        );
      }
      const [target, typmod, varcharLen] = resolveTypeAndTypmod(e.typeName, e.typeMod);
      // A string LITERAL operand is coerced to the target at resolve — CAST('42' AS int), the same
      // primitive as the `type 'string'` typed literal (grammar.md §36, types.md §5). The ONLY
      // text→T cast admitted ahead of the general cast slice; a non-literal text operand still
      // falls through to the deferred 0A000 below. A varchar(n) target truncates the literal to n
      // code points (types.md §15).
      if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
        // 'today'::date / CAST('now' AS date) — the clock-relative specials become the STABLE
        // dateClock node, exactly like the ctx-adaptation form (date.md §6).
        if (isDate(target)) {
          const clock = dateClockLiteral(e.inner.literal.text, params);
          if (clock !== null) return clock;
        }
        return coerceStringLiteral(e.inner.literal.text, target, typmod, varcharLen);
      }
      // Text casts are deferred (not in the cast matrix — spec/design/types.md §5/§11), EXCEPT
      // json/jsonb → text (the JSON cast matrix, json.md §6.1): json → text is the identity on the
      // verbatim bytes, jsonb → text renders the canonical form. A NULL adapts. Every other text
      // cast target is still a 0A000 this slice — including `$1::text` (declaring a bind param as
      // text via a cast stays deferred, the params contract — guarded FIRST so it does not resolve
      // to an untyped-NULL text node and trip 42P18).
      if (isText(target)) {
        if (e.inner.kind === "param") {
          throw engineError("feature_not_supported", "casting to text is not supported yet");
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        const ik = inner.type.kind;
        // A NULL adapts (NULL → NULL, no truncation needed).
        if (ik === "null") {
          return { node: inner.node, type: { kind: "text" } };
        }
        // text → text: the identity, UNLESS a varchar(n) length is present — then it becomes a real
        // cast node that silently truncates to n code points at eval (types.md §15).
        if (ik === "text") {
          if (varcharLen !== null) {
            return {
              node: { kind: "cast", target, typmod: null, varcharLen, operand: inner.node },
              type: { kind: "text" },
            };
          }
          return { node: inner.node, type: { kind: "text" } };
        }
        // json/jsonb → text (the JSON cast matrix) and uuid → text (the uuid cast slice,
        // casts.toml/types.md §14: canonical lowercase 8-4-4-4-12). Explicit — stricter than PG's
        // assignment-cast-to-text (a documented divergence). A varchar(n) length truncates the result.
        if (ik === "json" || ik === "jsonb" || ik === "uuid") {
          return {
            node: { kind: "cast", target, typmod, varcharLen, operand: inner.node },
            type: { kind: "text" },
          };
        }
        // array → text (spec/design/array.md §7): array_out renders {…} per row. Explicit-only, like
        // uuid/json → text (stricter than PG's assignment cast). Handled by arrayCast (toElem null).
        if (ik === "array") {
          return {
            node: { kind: "arrayCast", toElem: null, operand: inner.node },
            type: { kind: "text" },
          };
        }
        throw engineError("feature_not_supported", "casting to text is not supported yet");
      }
      // A boolean target (`CAST(x AS boolean)`, `x::boolean`) is the boolean cast slice
      // (spec/types/casts.toml, types.md §9). It needs the inner type to decide (only an i32 / NULL
      // / bool source is castable), so it is handled AFTER the inner is resolved, below.
      // A bytea TARGET: the uuid cast slice admits uuid → bytea (the 16 raw bytes — a jed cast PG
      // lacks; casts.toml, types.md §14). A string LITERAL was coerced above; a NULL adapts; a bytea
      // operand is the identity. text → bytea and every other bytea cast stay deferred (0A000 — the
      // bytea cast slice's own follow-on, types.md §13).
      if (isBytea(target)) {
        if (e.inner.kind === "param") {
          const pinner = resolve(scope, e.inner, "bytea", ag, params);
          return { node: pinner.node, type: { kind: "bytea" } };
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        const ik = inner.type.kind;
        if (ik === "null" || ik === "bytea") {
          return { node: inner.node, type: { kind: "bytea" } };
        }
        if (ik === "uuid") {
          return {
            node: { kind: "cast", target, typmod, varcharLen: null, operand: inner.node },
            type: { kind: "bytea" },
          };
        }
        throw engineError("feature_not_supported", "casting to bytea is not supported yet");
      }
      // The uuid cast slice (spec/types/casts.toml, types.md §14): a uuid TARGET from a runtime text
      // or bytea expression. text → uuid runs uuid_in at eval (22P02 on malformed); bytea → uuid takes
      // the 16 raw bytes (22P02 on a length ≠ 16) — a jed cast PG lacks. A string LITERAL operand was
      // coerced above (the §6 adaptation); $1::uuid declares the param as uuid; a NULL adapts; a uuid
      // operand is the identity.
      if (isUuid(target)) {
        if (e.inner.kind === "param") {
          const pinner = resolve(scope, e.inner, "uuid", ag, params);
          return { node: pinner.node, type: { kind: "uuid" } };
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        const ik = inner.type.kind;
        if (ik === "null" || ik === "uuid") {
          return { node: inner.node, type: { kind: "uuid" } };
        }
        if (ik === "text" || ik === "bytea") {
          return {
            node: { kind: "cast", target, typmod, varcharLen: null, operand: inner.node },
            type: { kind: "uuid" },
          };
        }
        throw typeError("cannot cast " + rtName(inner.type) + " to uuid");
      }
      // Cross-family datetime casts (timezones.md §9.3): a timestamp/timestamptz/date TARGET from
      // another datetime family. A same-family cast is the identity; a cross-family cast becomes a
      // dateConvert node (the zone-crossing ones read the session zone at eval); any non-datetime
      // source is the deferred 0A000. A NULL operand adapts to the target. text↔datetime casts stay
      // deferred and fall through (a non-datetime source is rejected here).
      if (isTimestamp(target) || isTimestamptz(target) || isDate(target)) {
        if (e.inner.kind === "param") {
          const pinner = resolve(scope, e.inner, target, ag, params);
          return { node: pinner.node, type: resolvedTypeOf(target) };
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        const toRt = resolvedTypeOf(target);
        const ik = inner.type.kind;
        if (ik === "null") return { node: inner.node, type: toRt };
        if (
          (ik === "timestamp" && isTimestamp(target)) ||
          (ik === "timestamptz" && isTimestamptz(target)) ||
          (ik === "date" && isDate(target))
        ) {
          return { node: inner.node, type: inner.type };
        }
        if (ik === "timestamp" || ik === "timestamptz" || ik === "date") {
          return { node: { kind: "dateConvert", inner: inner.node, to: target }, type: toRt };
        }
        if (ik === "text" && isDate(target)) {
          // The runtime text → date cast (date.md §6): a NON-literal text source (a string
          // LITERAL operand was already folded by the literal-adaptation path above) parses per
          // row via the same parseDate the literal uses (22007/22008 per row). STABLE, not
          // immutable — the input grammar admits the clock-relative specials — so it flags the
          // plan non-immutable (42P17 in an index expression, as in PG). text → timestamp /
          // timestamptz stays deferred (the throw below).
          params.nonimmutable = true;
          return { node: { kind: "dateConvert", inner: inner.node, to: target }, type: toRt };
        }
        throw engineError(
          "feature_not_supported",
          `cannot cast ${rtName(inner.type)} to ${canonicalName(target)}`,
        );
      }
      // interval casts are deferred (spec/design/interval.md): casting TO interval is 0A000.
      if (isInterval(target)) {
        throw engineError(
          "feature_not_supported",
          "casting to an interval type is not supported yet",
        );
      }
      // The JSON cast matrix (spec/design/json.md §6.1): casting TO json/jsonb from a runtime
      // text/json/jsonb expression (a string LITERAL operand was already coerced above by
      // coerceStringLiteral). text → json validates + stores verbatim; text → jsonb parses +
      // canonicalizes; json → jsonb re-parses + canonicalizes; jsonb → json renders the canonical
      // text; same-type is the identity. Any other source is a 42804 cast error (jed's invalid-cast
      // convention; PG reports 42846 — a documented divergence).
      if (isJson(target) || isJsonb(target)) {
        if (e.inner.kind === "param") {
          const pinner = resolve(scope, e.inner, target, ag, params);
          return { node: pinner.node, type: resolvedTypeOf(target) };
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        const toRt = resolvedTypeOf(target);
        const ik = inner.type.kind;
        if (ik === "null") return { node: inner.node, type: toRt };
        if (ik === "text" || ik === "json" || ik === "jsonb") {
          return {
            node: { kind: "cast", target, typmod, varcharLen: null, operand: inner.node },
            type: toRt,
          };
        }
        throw typeError("cannot cast type " + rtName(inner.type) + " to " + canonicalName(target));
      }
      // A bind-parameter operand takes the cast TARGET as its inferred type — `$1::int` (and
      // `CAST($1 AS int)`) declares `$1` as int, the cast-target parameter-typing case
      // (spec/design/api.md §5, grammar.md §37). Every other operand resolves with NO literal
      // context (its value is range-checked / coerced against target at eval), so changing the
      // context only for a parameter leaves all existing CAST behavior untouched.
      // A boolean target accepts only an i32 source (the boolean cast slice): an untyped integer
      // literal operand adapts to i32 (CAST(5 AS boolean) / 5::boolean), matching PG. A column/
      // expression keeps its own type; a literal beyond i32 range then traps 22003 (PG 42846 — a
      // documented divergence).
      const innerCtx = e.inner.kind === "param" ? target : isBool(target) ? "i32" : null;
      const inner = resolve(scope, e.inner, innerCtx, ag, params);
      // The boolean cast slice (spec/types/casts.toml, types.md §9): PG ties boolean↔integer to i32
      // ONLY and makes both directions explicit. A boolean TARGET takes an i32 / NULL / bool source
      // (the eval maps 0→false, nonzero→true); a boolean SOURCE produces an i32 (true→1, false→0).
      // Handled here, ahead of the generic numeric cast below — resultType assumes an int/decimal/
      // float target, so a boolean target must not fall through. A bool⇄i16 / bool⇄i64 pair is a
      // forbidden 42804 (jed's datatype-mismatch convention; PG reports 42846, casts.toml).
      if (isBool(target)) {
        // A runtime `text` source is the runtime-text-cast slice (grammar.md §36): the eval parses
        // the per-row string via the same parseBoolLiteral (PG boolin) the 't'::boolean literal
        // uses. A string LITERAL operand was already coerced above, so a text source here is
        // non-literal (a column / expression).
        if (
          (inner.type.kind === "int" && inner.type.ty === "i32") ||
          inner.type.kind === "null" ||
          inner.type.kind === "bool" ||
          inner.type.kind === "text"
        ) {
          return {
            node: { kind: "cast", target, typmod, varcharLen: null, operand: inner.node },
            type: { kind: "bool" },
          };
        }
        throw typeError("cannot cast " + rtName(inner.type) + " to boolean");
      }
      if (inner.type.kind === "bool") {
        // boolean → i32 is the one boolean-source cast; any other target is forbidden (42804).
        if (target === "i32") {
          return {
            node: { kind: "cast", target, typmod, varcharLen: null, operand: inner.node },
            type: { kind: "int", ty: "i32" },
          };
        }
        throw typeError("cannot cast boolean to " + canonicalName(target));
      }
      // A runtime `text` source to a numeric target is the runtime-text-cast slice (grammar.md
      // §36): the only targets reaching this generic path are int / decimal / float (text / bytea /
      // uuid / datetime / interval / bool / json targets all return in their own blocks above), so
      // a text source here casts to a number. The eval coerces the per-row string via the same
      // parse functions the literal form uses (22P02 / 22003 per row). A string LITERAL operand was
      // already folded above, so this text is non-literal — fall through to the numeric cast node.
      // Casting FROM bytea is likewise deferred (0A000).
      if (inner.type.kind === "bytea") {
        throw engineError("feature_not_supported", "casting from bytea is not supported yet");
      }
      // Casting FROM uuid is likewise deferred (0A000).
      if (inner.type.kind === "uuid") {
        throw engineError("feature_not_supported", "casting from uuid is not supported yet");
      }
      // Casting FROM a timestamp is likewise deferred (0A000).
      if (inner.type.kind === "timestamp" || inner.type.kind === "timestamptz") {
        throw engineError(
          "feature_not_supported",
          "casting from a timestamp type is not supported yet",
        );
      }
      // Casting FROM an interval is likewise deferred (0A000).
      if (inner.type.kind === "interval") {
        throw engineError(
          "feature_not_supported",
          "casting from an interval type is not supported yet",
        );
      }
      // Casting FROM a date is likewise deferred (0A000; date↔timestamp unblocks the cross-family comparison — date.md §4/§6).
      if (inner.type.kind === "date") {
        throw engineError("feature_not_supported", "casting from a date type is not supported yet");
      }
      // Casting FROM an array (array→text, element-wise array→array) is deferred (array.md §7/§12).
      if (inner.type.kind === "array") {
        throw engineError("feature_not_supported", "casting an array value is not supported yet");
      }
      // Casting FROM json/jsonb (json↔jsonb, json[b]→text) lands in J3 (spec/design/json.md §6);
      // deferred this slice. Casting FROM jsonpath is likewise deferred.
      if (
        inner.type.kind === "json" ||
        inner.type.kind === "jsonb" ||
        inner.type.kind === "jsonpath"
      ) {
        throw engineError("feature_not_supported", "casting a json value is not supported yet");
      }
      // int→int (range check), int→decimal (widen), decimal→int (explicit, round),
      // decimal→decimal (re-scale), the float casts (int↔float, decimal↔float, float↔float — all
      // explicit, float.md §6), and NULL are all castable. The CAST matrix (casts.toml) is strict:
      // these are exactly the legal (from, to) pairs across the int/decimal/float families.
      const resultType: ResolvedType = isDecimal(target)
        ? { kind: "decimal" }
        : isFloat(target)
          ? { kind: "float", ty: target }
          : { kind: "int", ty: target };
      return {
        node: { kind: "cast", target, typmod, varcharLen: null, operand: inner.node },
        type: resultType,
      };
    }
    case "unary":
      if (e.op === "neg") {
        const { node, type } = resolve(scope, e.operand, ctx, ag, params);
        if (type.kind === "decimal") {
          return {
            node: { kind: "neg", result: "decimal", operand: node },
            type: { kind: "decimal" },
          };
        }
        if (type.kind === "float") {
          // Unary minus on a float flips the sign bit (no overflow); a NaN/Inf operand passes
          // through per IEEE (-NaN is NaN, -Inf is -Inf) — float.md §5. result keeps the width.
          return {
            node: { kind: "neg", result: type.ty, operand: node },
            type: { kind: "float", ty: type.ty },
          };
        }
        if (type.kind === "interval") {
          // -interval (spec/design/interval.md §5).
          return {
            node: { kind: "neg", result: "interval", operand: node },
            type: { kind: "interval" },
          };
        }
        let result: ScalarType;
        if (type.kind === "int") result = type.ty;
        else if (type.kind === "null")
          result = "i64"; // -NULL = NULL
        else throw typeError("unary minus requires a numeric operand");
        return {
          node: { kind: "neg", result, operand: node },
          type: { kind: "int", ty: result },
        };
      }
      {
        const { node, type } = resolve(scope, e.operand, null, ag, params);
        requireBool(type, "NOT requires a boolean operand");
        return { node: { kind: "not", operand: node }, type: { kind: "bool" } };
      }
    case "isNull": {
      const { node } = resolve(scope, e.operand, null, ag, params);
      return {
        node: { kind: "isNull", operand: node, negated: e.negated },
        type: { kind: "bool" },
      };
    }
    case "isJson": {
      // The operand must be a character string / json / jsonb (else 42804); a bare string literal
      // resolves as text. The predicate is always a definite boolean (a NULL operand → NULL at eval).
      const { node, type } = resolve(scope, e.operand, null, ag, params);
      switch (type.kind) {
        case "text":
        case "json":
        case "jsonb":
        case "null":
          break;
        default:
          throw engineError(
            "datatype_mismatch",
            "cannot use type " + rtName(type) + " in IS JSON predicate",
          );
      }
      return {
        node: {
          kind: "isJson",
          operand: node,
          negated: e.negated,
          jsonKind: e.jsonKind,
          uniqueKeys: e.uniqueKeys,
        },
        type: { kind: "bool" },
      };
    }
    case "jsonCtor": {
      // JSON(text) parses a character string to a `json` value (verbatim). The operand must be text
      // (a bare string literal stays text under the Text context hint); a non-text operand → 42804.
      const { node, type } = resolve(scope, e.operand, "text", ag, params);
      switch (type.kind) {
        case "text":
        case "null":
          break;
        default:
          throw engineError(
            "datatype_mismatch",
            "cannot use type " + rtName(type) + " as JSON() input",
          );
      }
      return {
        node: { kind: "jsonCtor", operand: node, uniqueKeys: e.uniqueKeys },
        type: { kind: "json" },
      };
    }
    // The SQL/JSON query functions JSON_EXISTS / JSON_VALUE / JSON_QUERY (json-sql-functions.md §5,
    // S2). Each compiles a jsonpath, evaluates it over a context item, and applies per-function
    // semantics (the existence predicate / a single scalar / a json value). resolveJsonSqlFn does the
    // shared context/path resolution + the RETURNING/behavior bookkeeping.
    case "jsonExists":
      return resolveJsonSqlFn(
        scope,
        "exists",
        e.ctx,
        e.path,
        null,
        "without",
        true,
        null,
        e.onError,
        ag,
        params,
      );
    case "jsonValue":
      return resolveJsonSqlFn(
        scope,
        "value",
        e.ctx,
        e.path,
        e.returning,
        "without",
        true,
        e.onEmpty,
        e.onError,
        ag,
        params,
      );
    case "jsonQuery":
      return resolveJsonSqlFn(
        scope,
        "query",
        e.ctx,
        e.path,
        e.returning,
        e.wrapper,
        e.keepQuotes,
        e.onEmpty,
        e.onError,
        ag,
        params,
      );
    case "isDistinct": {
      // NULL-safe equality: the SAME operand contract as `=` — resolve the pair (a literal
      // adapts to its sibling; a text literal stays text), then require the operands be
      // comparable (both integer-ish or both text-ish; a mixed pair is 42804). The result
      // is always a definite boolean (functions.md §3).
      const p = resolveOperandPair(scope, e.lhs, e.rhs, ag, params);
      classifyComparable(p.lt, p.rt);
      return {
        node: { kind: "distinct", lhs: p.rl, rhs: p.rr, negated: e.negated },
        type: { kind: "bool" },
      };
    }
    case "binary":
      return resolveBinary(scope, e.op, e.lhs, e.rhs, ag, params);
    case "quantified":
      return resolveQuantified(scope, e.op, e.all, e.lhs, e.array, ag, params);
    case "in": {
      // An EMPTY list reaches here only from folding an IN-subquery whose result was empty
      // (grammar.md §26; the parser rejects literal `IN ()` → 42601). The value is a constant —
      // `x IN (empty)` = FALSE, `x NOT IN (empty)` = TRUE — for every x including NULL. Still
      // resolve the LHS so an undefined column / aggregate-context error fires, then return the
      // constant (a leaf — no operator_eval, cost.md §3).
      if (e.list.length === 0) {
        resolve(scope, e.lhs, null, ag, params);
        return {
          node: { kind: "constBool", value: e.negated },
          type: { kind: "bool" },
        };
      }
      // Desugar to the OR-chain PostgreSQL DEFINES `IN` as: `x IN (a,b,c)` is
      // `x = a OR x = b OR x = c`; `NOT IN` is its negation (grammar.md §20). The list is
      // non-empty (the parser rejects `IN ()` → 42601). Resolving the desugared tree reuses the
      // `=`/OR/NOT machinery verbatim, so the three-valued NULL semantics, per-element operand
      // typing (a too-wide literal → 22003, a cross-family element → 42804), and cost all fall
      // out. The LHS is evaluated once per element (the OR-chain model — a documented cost
      // consequence, cost.md §3).
      let folded: Expr | null = null;
      for (const elem of e.list) {
        const eq: Expr = { kind: "binary", op: "eq", lhs: e.lhs, rhs: elem };
        folded = folded === null ? eq : { kind: "binary", op: "or", lhs: folded, rhs: eq };
      }
      // folded is non-null: the parser guarantees a non-empty list.
      let desugared = folded as Expr;
      if (e.negated) {
        desugared = { kind: "unary", op: "not", operand: desugared };
      }
      return resolve(scope, desugared, ctx, ag, params);
    }
    case "between": {
      // Desugar to `lhs >= lo AND lhs <= hi` (grammar.md §21). The Kleene AND gives the PG
      // result for a NULL bound: `5 BETWEEN 10 AND NULL` is `FALSE AND NULL` = FALSE (a FALSE
      // operand dominates), while `5 BETWEEN 1 AND NULL` is `TRUE AND NULL` = NULL. NOT BETWEEN
      // negates the whole conjunction. The LHS is evaluated twice (the desugar model — a
      // documented cost consequence, cost.md §3).
      const ge: Expr = { kind: "binary", op: "ge", lhs: e.lhs, rhs: e.lo };
      const le: Expr = { kind: "binary", op: "le", lhs: e.lhs, rhs: e.hi };
      let desugared: Expr = { kind: "binary", op: "and", lhs: ge, rhs: le };
      if (e.negated) {
        desugared = { kind: "unary", op: "not", operand: desugared };
      }
      return resolve(scope, desugared, ctx, ag, params);
    }
    case "like": {
      // LIKE / ILIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal
      // stays text), then require BOTH operands be text (or a bare NULL); a non-text operand is
      // 42804. We do NOT use classifyComparable here — it would wrongly accept bytea×bytea.
      const p = resolveOperandPair(scope, e.lhs, e.rhs, ag, params);
      requireTextOrNull(p.lt);
      requireTextOrNull(p.rt);
      return {
        node: {
          kind: "like",
          lhs: p.rl,
          rhs: p.rr,
          negated: e.negated,
          insensitive: e.insensitive,
        },
        type: { kind: "bool" },
      };
    }
    case "regex": {
      // ~ / ~* / !~ / !~* — text×text → boolean (grammar.md §22b, regex.md). Same operand typing as
      // LIKE: resolve the pair, require both text (or a bare NULL); a non-text operand is 42804.
      const p = resolveOperandPair(scope, e.lhs, e.rhs, ag, params);
      requireTextOrNull(p.lt);
      requireTextOrNull(p.rt);
      // Precompile a CONSTANT pattern ONCE (regex.md §5); a non-constant pattern compiles per row at
      // eval. For ~* the constant is case-folded before compiling (the ILIKE mechanism). A malformed
      // pattern surfaces 2201B (and an oversized one 54001) here, at resolve, for the constant case.
      let program: RegexProgram | null = null;
      if (p.rr.kind === "constText") {
        const pat = e.insensitive ? foldLowerSimple(p.rr.value, loadedProperty()) : p.rr.value;
        program = compileRegex(pat);
      }
      // A precompiled program carries the one-shot compileCharged cost flag mutated on first eval, so
      // a reused plan would under-charge the 2nd+ execute — never cache such a plan.
      if (program !== null) params.uncacheable = true;
      return {
        node: {
          kind: "regex",
          lhs: p.rl,
          rhs: p.rr,
          negated: e.negated,
          insensitive: e.insensitive,
          program,
          compileCharged: false,
        },
        type: { kind: "bool" },
      };
    }
    case "case": {
      // Resolve each branch's condition: searched form requires a boolean WHEN (42804
      // otherwise); simple form desugars to `operand = value` (reusing the `=` operand pairing +
      // comparability check, so the value adapts to the operand's type). The operand is evaluated
      // once per tested branch (the desugar model, like IN).
      const arms: { cond: RExpr; result: RExpr }[] = [];
      const resultTypes: ResolvedType[] = [];
      for (const w of e.whens) {
        let cond: RExpr;
        if (e.operand !== null) {
          const eq: Expr = {
            kind: "binary",
            op: "eq",
            lhs: e.operand,
            rhs: w.cond,
          };
          cond = resolve(scope, eq, null, ag, params).node;
        } else {
          const rc = resolve(scope, w.cond, null, ag, params);
          requireBool(rc.type, "CASE WHEN condition must be boolean");
          cond = rc.node;
        }
        const rres = resolve(scope, w.result, null, ag, params);
        resultTypes.push(rres.type);
        arms.push({ cond, result: rres.node });
      }
      let els: RExpr;
      if (e.els !== null) {
        const re = resolve(scope, e.els, null, ag, params);
        els = re.node;
        resultTypes.push(re.type);
      } else {
        els = { kind: "constNull" };
        resultTypes.push({ kind: "null" });
      }
      const unified = unifyCaseTypes(resultTypes, "CASE result types must be compatible");
      return {
        node: {
          kind: "case",
          arms,
          els,
          coerceDecimal: unified.kind === "decimal",
        },
        type: unified,
      };
    }
    case "coalesce": {
      // COALESCE(a, b, …) (grammar.md §51): each argument resolves in the same agg context (an
      // aggregate argument is legal wherever an aggregate is), and the argument types unify to
      // one common type exactly like CASE's result arms.
      const args: RExpr[] = [];
      const argTypes: ResolvedType[] = [];
      for (const a of e.args) {
        const ra = resolve(scope, a, null, ag, params);
        args.push(ra.node);
        argTypes.push(ra.type);
      }
      const unified = unifyCaseTypes(argTypes, "COALESCE types must be compatible");
      return {
        node: {
          kind: "coalesce",
          args,
          coerceDecimal: unified.kind === "decimal",
        },
        type: unified,
      };
    }
    case "greatestLeast": {
      // GREATEST/LEAST(a, b, …) (grammar.md §52): each argument resolves in the same agg context,
      // and the argument types unify to one common ORDERABLE type. The winner is chosen by that
      // type's total order at eval, so — unlike CASE/COALESCE, which never compare — the common
      // type must actually be comparable and mixed-width floats must be widened (numeric promote,
      // float widths to the widest, other families structural); a non-orderable common type
      // (json/jsonpath) or an incomparable pair is rejected by classifyComparable HERE, never
      // silently mis-ordered by valueCmp's cross-family totality fallback.
      const name = e.greatest ? "greatest" : "least";
      const resolved = e.args.map((a) => resolve(scope, a, null, ag, params));
      const types = resolved.map((r) => r.type);
      const nonNull = types.filter((t) => t.kind !== "null");
      let unified: ResolvedType;
      if (nonNull.length === 0) {
        unified = { kind: "text" };
      } else if (nonNull.every((t) => t.kind === "int" || t.kind === "decimal")) {
        if (nonNull.some((t) => t.kind === "decimal")) {
          unified = { kind: "decimal" };
        } else {
          let ty = (nonNull[0] as { kind: "int"; ty: ScalarType }).ty;
          for (const t of nonNull.slice(1)) {
            const next = (t as { kind: "int"; ty: ScalarType }).ty;
            if (rank(next) > rank(ty)) ty = next;
          }
          unified = { kind: "int", ty };
        }
      } else if (nonNull.every((t) => t.kind === "float")) {
        let ty = (nonNull[0] as { kind: "float"; ty: ScalarType }).ty;
        for (const t of nonNull.slice(1)) {
          ty = promoteFloat(ty, (t as { kind: "float"; ty: ScalarType }).ty);
        }
        unified = { kind: "float", ty };
      } else {
        unified = nonNull[0]!;
        for (const t of nonNull.slice(1)) {
          if (!resolvedTypeEqual(unified, t)) {
            throw typeError(`${name.toUpperCase()} types must be compatible`);
          }
        }
      }
      classifyComparable(unified, unified);
      // A bare parameter takes the unified scalar type (like CASE/COALESCE — grammar.md §42).
      const hint = scalarForParamHint(unified);
      for (const a of e.args) {
        if (a.kind === "param") params.note(a.index - 1, hint);
      }
      // A mixed-width float set unifies to f64; widen the f32 arguments (an ordinary cast, whose
      // cost stays observable) so the comparator sees one width.
      const args = resolved.map((r) =>
        unified.kind === "float" && r.type.kind === "float"
          ? widenFloatTo(r.node, r.type.ty, unified.ty)
          : r.node,
      );
      // Text arguments derive one comparison collation (42P21/42P22 on conflict — §52).
      let collation: Collation | null = null;
      if (unified.kind === "text") {
        let deriv: Deriv = { kind: "none" };
        for (const a of e.args) deriv = combineDeriv(deriv, deriveCollation(scope, a));
        collation = resolveDeriv(scope.catalog, deriv);
      }
      return {
        node: {
          kind: "greatestLeast",
          args,
          coerceDecimal: unified.kind === "decimal",
          greatest: e.greatest,
          collation,
        },
        type: unified,
      };
    }
  }
}

// resolveCollationName resolves a collation NAME to its table (spec/design/collation.md §1). C is the
// built-in byte / code-point order → null (the unchanged fast path); any other name resolves through
// the reference-only read path (the database's resolved set, then the binary's vendored set), else
// 42704.
export function resolveCollationName(catalog: Engine, name: string): Collation | null {
  if (name === "C") return null;
  const c = catalog.resolveCollationByName(name);
  if (c === undefined) {
    throw engineError("undefined_object", `collation "${name}" does not exist`);
  }
  return c;
}

// A text expression's collation derivation (spec/design/collation.md §1, PG's rules). "none" = no
// collation (a non-text expr or a bare literal); "implicit" = a column's frozen collation (C counts
// as a distinct implicit collation); "explicit" = an explicit COLLATE; "indeterminate" = two
// different implicit collations met — 42P22 when consumed.
export type Deriv =
  | { kind: "none" }
  | { kind: "implicit"; name: string }
  | { kind: "explicit"; name: string }
  | { kind: "indeterminate" };

// deriveCollation derives the collation + derivation level of a (text) expression subtree. A COLLATE
// is explicit; a column reference is implicit (its frozen collation, C if none); || combines its
// operands. Every other shape resets to none (takes a neighbour's) — a documented narrowing (§14).
export function deriveCollation(scope: Scope, e: Expr): Deriv {
  if (e.kind === "collate") return { kind: "explicit", name: e.collation };
  if (e.kind === "column") return columnDeriv(scope, () => scope.resolveBare(e.name));
  if (e.kind === "qualifiedColumn") {
    return columnDeriv(scope, () => scope.resolveQualified(e.qualifier, e.name));
  }
  if (e.kind === "binary" && e.op === "concat") {
    return combineDeriv(deriveCollation(scope, e.lhs), deriveCollation(scope, e.rhs));
  }
  return { kind: "none" };
}

// columnDeriv is the implicit derivation of a resolved column reference: a text column carries its
// frozen collation (C → "C", a distinct implicit collation); a non-text or unresolvable reference
// is "none".
export function columnDeriv(scope: Scope, resolve: () => Resolved): Deriv {
  let col: Column;
  try {
    col = scope.columnOf(resolve());
  } catch {
    return { kind: "none" };
  }
  if (!typeIsText(col.type)) return { kind: "none" };
  return { kind: "implicit", name: col.collation ?? "C" };
}

// combineDeriv combines two operands' derivations (spec/design/collation.md §1/§7, PG's rules).
// Explicit dominates; two DIFFERENT explicit collations conflict eagerly (42P21); two different
// implicit collations yield "indeterminate" (deferred to 42P22 on use); explicit resolves it.
export function combineDeriv(a: Deriv, b: Deriv): Deriv {
  if (a.kind === "explicit" && b.kind === "explicit") {
    if (a.name !== b.name) {
      throw engineError(
        "collation_mismatch",
        `collation mismatch between explicit collations "${a.name}" and "${b.name}"`,
      );
    }
    return a;
  }
  if (a.kind === "explicit") return a;
  if (b.kind === "explicit") return b;
  if (a.kind === "indeterminate" || b.kind === "indeterminate") {
    return { kind: "indeterminate" };
  }
  if (a.kind === "implicit" && b.kind === "implicit") {
    return a.name === b.name ? a : { kind: "indeterminate" };
  }
  if (a.kind === "implicit") return a;
  return b;
}

// resolveDeriv resolves a derivation to the concrete collation a comparison / ORDER BY uses. "none"
// and C → null (byte order, the fast path); a loaded name → its table (42704 if it vanished);
// "indeterminate" → 42P22 (the collation is required but ambiguous).
export function resolveDeriv(catalog: Engine, d: Deriv): Collation | null {
  if (d.kind === "indeterminate") {
    throw engineError(
      "indeterminate_collation",
      "could not determine which collation to use for string comparison",
    );
  }
  if (d.kind === "implicit" || d.kind === "explicit") {
    return resolveCollationName(catalog, d.name);
  }
  return null;
}

// collatedCmp compares two non-NULL text values under a loaded collation (spec/design/collation.md
// §6/§7): order by the UCA sort keys, whose memcmp order IS the collation order. The caller charges
// the collate cost and handles NULLs. Returns <0, 0, >0.
export function collatedCmp(coll: Collation, a: string, b: string): number {
  return cmpBytes(collationSortKey(coll, a), collationSortKey(coll, b));
}

export function resolveBinary(
  scope: Scope,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  switch (op) {
    case "add":
    case "sub":
    case "mul":
    case "div":
    case "mod": {
      // jsonb `-` is the delete operator (json-sql-functions.md §1, J6), NOT arithmetic — its right
      // operand is a key/index/keys, never an arithmetic value. Peek the LHS type; a jsonb LHS with
      // `-` routes to the delete resolver. (Only `-` has a jsonb meaning; `+ * / %` over a jsonb
      // operand fall through and 42804 in the numeric path.)
      if (op === "sub") {
        const peek = resolve(scope, lhs, null, ag, params);
        if (peek.type.kind === "jsonb") {
          return resolveJsonbDelete(scope, false, rhs, peek.node, ag, params);
        }
      }
      // Arithmetic is overloaded across integer and decimal. Resolve the operand pair (an
      // integer literal adapts to an integer sibling), then pick the family: both integer →
      // integer arithmetic; at least one decimal → decimal arithmetic (the integer operand
      // widens at eval); a text/boolean operand is a 42804 (spec/design/decimal.md §4).
      const p = resolveOperandPair(scope, lhs, rhs, ag, params);
      // Range set operators (RF4, spec/design/range-functions.md §4): `+` union, `-` difference, `*`
      // intersection over two ranges. A range operand in any of these three is the set-op axis — both
      // operands must be ranges of a common element type, else 42883 (matching PG's "operator does not
      // exist"); the numeric/temporal arithmetic below never sees a range. `/` and `%` have no range
      // meaning and fall straight through.
      if (
        (op === "add" || op === "sub" || op === "mul") &&
        (p.lt.kind === "range" || p.rt.kind === "range")
      ) {
        return resolveRangeSetOp(op, p.rl, p.lt, p.rr, p.rt);
      }
      // Date arithmetic (spec/design/date.md §6): date ± int → date, date − date → i32 (days
      // between), date ± interval → timestamp. Checked BEFORE the interval/timestamp rules below:
      // a `date ± interval` pair has an interval operand, which would otherwise make
      // temporalArithResult throw a 42804 (date is not one of its temporal kinds). Any other
      // arithmetic combination involving a date throws a 42804 from dateArithResult.
      if (p.lt.kind === "date" || p.rt.kind === "date") {
        const dresult = dateArithResult(op, p.lt.kind, p.rt.kind);
        return {
          node: { kind: "arith", op, result: dresult, lhs: p.rl, rhs: p.rr },
          type: resolvedTypeOf(dresult),
        };
      }
      // interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5). Checked
      // before the ±-only temporal rule below.
      const scaled = intervalScaleResult(op, p.lt.kind, p.rt.kind);
      if (scaled !== undefined) {
        return {
          node: { kind: "arith", op, result: scaled, lhs: p.rl, rhs: p.rr },
          type: resolvedTypeOf(scaled),
        };
      }
      // Temporal arithmetic (spec/design/interval.md §5): interval ± interval, timestamp[tz] ±
      // interval, interval + timestamp[tz], and timestamp[tz] − timestamp[tz] → interval. The
      // eval dispatches on the value kinds; here we settle the result type. A temporal operand in
      // any other combination is a 42804.
      const temporal = temporalArithResult(op, p.lt.kind, p.rt.kind);
      if (temporal !== undefined) {
        return {
          node: { kind: "arith", op, result: temporal, lhs: p.rl, rhs: p.rr },
          type: resolvedTypeOf(temporal),
        };
      }
      // Float arithmetic (float.md §5): float ⊕ float → float for + - * / % (and unary - via the
      // neg path). A mixed-width pair PROMOTES to f64 (the higher rank), so the computation is
      // always at one width. NO cross-family promotion — int/decimal ⊕ float is 42804 (a float
      // operand with a non-float, non-null sibling falls through to requireNumericOperand, which
      // does NOT accept float, raising the type error). A float literal sibling already adapted via
      // ctxOf, so a literal+float pair is float×float here.
      if (p.lt.kind === "float" || p.rt.kind === "float") {
        if (p.lt.kind !== "float" || p.rt.kind !== "float") {
          throw typeError("arithmetic operators require operands of the same family");
        }
        const result = promoteFloat(p.lt.ty, p.rt.ty);
        const lhsW = widenFloatTo(p.rl, p.lt.ty, result);
        const rhsW = widenFloatTo(p.rr, p.rt.ty, result);
        return {
          node: { kind: "arith", op, result, lhs: lhsW, rhs: rhsW },
          type: { kind: "float", ty: result },
        };
      }
      requireNumericOperand(p.lt);
      requireNumericOperand(p.rt);
      if (p.lt.kind === "decimal" || p.rt.kind === "decimal") {
        return {
          node: { kind: "arith", op, result: "decimal", lhs: p.rl, rhs: p.rr },
          type: { kind: "decimal" },
        };
      }
      const result = promote(p.lt, p.rt);
      return {
        node: { kind: "arith", op, result, lhs: p.rl, rhs: p.rr },
        type: { kind: "int", ty: result },
      };
    }
    case "eq":
    case "ne":
    case "lt":
    case "gt":
    case "le":
    case "ge": {
      // Comparison is overloaded across families: integer×integer or text×text. Resolve the
      // operands (a literal adapts to its sibling; text literals stay text), then require
      // they be comparable — a mixed integer/text pair is 42804. The runtime comparison
      // (eq3/lt3/gt3) dispatches on the value kinds.
      const p = resolveOperandPair(scope, lhs, rhs, ag, params);
      classifyComparable(p.lt, p.rt);
      // A mixed-width float comparison promotes the narrower operand to f64 (float.md §3), so
      // the runtime eq3/lt3/gt3 see one width (they require both sides the same float kind).
      let cl = p.rl;
      let cr = p.rr;
      if (p.lt.kind === "float" && p.rt.kind === "float") {
        const w = promoteFloat(p.lt.ty, p.rt.ty);
        cl = widenFloatTo(p.rl, p.lt.ty, w);
        cr = widenFloatTo(p.rr, p.rt.ty, w);
      }
      // Derive the comparison's collation (spec/design/collation.md §1/§7). Only a text×text
      // comparison is collatable; a COLLATE on a non-text operand was already rejected 42804 at the
      // collate node. Each operand's derivation (explicit COLLATE / implicit column collation / none)
      // is combined per PG's rules: two different EXPLICIT collations conflict (42P21); two different
      // IMPLICIT collations are indeterminate (42P22 when consumed here). Derived for ALL comparison
      // ops incl =/<> (PG raises regardless), even though =/<> ignore the collation at eval (byte
      // equality, §7).
      let collation: Collation | null = null;
      if (p.lt.kind === "text" && p.rt.kind === "text") {
        const d = combineDeriv(deriveCollation(scope, lhs), deriveCollation(scope, rhs));
        collation = resolveDeriv(scope.catalog, d);
      }
      return {
        node: { kind: "compare", op, lhs: cl, rhs: cr, collation },
        type: { kind: "bool" },
      };
    }
    case "concat":
      return resolveConcat(scope, lhs, rhs, ag, params);
    // The containment/overlap operators (@>/<@/&&, shared by arrays and ranges) and the five
    // range-only positional/adjacency operators (<</>>/&</&>/-|-) all dispatch here: the operand
    // type chooses the array axis (array-functions.md §10) or the range axis (range-functions.md §3).
    case "contains":
    case "containedBy":
    case "overlaps":
    case "strictlyLeft":
    case "strictlyRight":
    case "notExtendRight":
    case "notExtendLeft":
    case "adjacent":
      return resolveSetOp(scope, op, lhs, rhs, ag, params);
    // The jsonb accessor operators (spec/design/json-sql-functions.md §1, J4).
    case "jsonGet":
    case "jsonGetText":
    case "jsonGetPath":
    case "jsonGetPathText":
      return resolveJsonAccess(scope, op, lhs, rhs, ag, params);
    // The jsonb key-existence operators (spec/design/json-sql-functions.md §1, J5).
    case "jsonHasKey":
      return resolveJsonHasKey(scope, "one", lhs, rhs, ag, params);
    case "jsonHasAnyKey":
      return resolveJsonHasKey(scope, "any", lhs, rhs, ag, params);
    case "jsonHasAllKeys":
      return resolveJsonHasKey(scope, "all", lhs, rhs, ag, params);
    // The jsonb delete-at-path operator `#-` (spec/design/json-sql-functions.md §1, J6). `||` and
    // `-` (delete) are dispatched by operand type in resolveConcat / the arithmetic arm.
    case "jsonDeletePath": {
      const rbase = resolve(scope, lhs, "jsonb", ag, params);
      if (rbase.type.kind !== "jsonb" && rbase.type.kind !== "null") {
        throw engineError(
          "undefined_function",
          `operator does not exist: ${rtName(rbase.type)} #- text[]`,
        );
      }
      return resolveJsonbDelete(scope, true, rhs, rbase.node, ag, params);
    }
    // `jsonb @? jsonpath` = jsonb_path_exists; `jsonb @@ jsonpath` is the silent form of
    // jsonb_path_match (jsonpath.md §6). Both reuse the jsonpath kernels.
    case "jsonPathExists":
    case "jsonPathMatch": {
      const [sym, kind]: [string, JsonPathFnKind] =
        op === "jsonPathExists" ? ["@?", "exists"] : ["@@", "matchSilent"];
      const ctx = resolve(scope, lhs, "jsonb", ag, params);
      if (ctx.type.kind !== "jsonb" && ctx.type.kind !== "null") {
        throw engineError(
          "undefined_function",
          `operator does not exist: ${rtName(ctx.type)} ${sym} jsonpath`,
        );
      }
      const path = resolve(scope, rhs, "jsonpath", ag, params);
      if (path.type.kind !== "jsonpath" && path.type.kind !== "null") {
        throw engineError(
          "undefined_function",
          `operator does not exist: jsonb ${sym} (a non-jsonpath)`,
        );
      }
      return {
        node: { kind: "jsonPathFn", pathFnKind: kind, args: [ctx.node, path.node] },
        type: { kind: "bool" },
      };
    }
    default: {
      // "and" | "or"
      const l = resolve(scope, lhs, null, ag, params);
      const r = resolve(scope, rhs, null, ag, params);
      requireBool(l.type, "AND/OR requires boolean operands");
      requireBool(r.type, "AND/OR requires boolean operands");
      return {
        node: { kind: op === "and" ? "and" : "or", lhs: l.node, rhs: r.node },
        type: { kind: "bool" },
      };
    }
  }
}

// resolveConcat resolves the `||` array concatenation operator (array-functions.md §8): overload
// resolution over the three kind=="concat" catalog rows — (anyarray,anyarray) [array_cat],
// (anyarray,anyelement) [array_append], (anyelement,anyarray) [array_prepend] — tried IN CATALOG
// ORDER, first match wins. It is the operator spelling of the AF1 builders and reuses their kernels.
//
// Two passes like resolveArrayFunc, with one deliberate difference: a BARE untyped NULL operand is
// left un-adapted. matchPoly defers a bare NULL in an anyarray slot, so cat-first makes `arr || NULL`
// / `NULL || arr` resolve to array_cat (the NULL array = identity), matching PostgreSQL; adapting the
// bare NULL to a typed element would wrongly steer it into array_append.
export function resolveConcat(
  scope: Scope,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const noOverload = (): EngineError =>
    engineError(
      "undefined_function",
      "operator does not exist: the || operands are not an array and a compatible element/array",
    );
  // Pass 1: resolve both operands with no hint.
  let rl = resolve(scope, lhs, null, ag, params);
  let rr = resolve(scope, rhs, null, ag, params);
  // JSONB axis: a jsonb operand routes `||` to jsonb concat/merge (json-sql-functions.md §1, J6).
  if (rl.type.kind === "jsonb" || rr.type.kind === "jsonb") {
    return resolveJsonbConcat(scope, lhs, rhs, ag, params);
  }
  // The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8).
  let hint: ScalarType | null = null;
  if (rl.type.kind === "array") hint = elemScalarHint(rl.type.elem);
  else if (rr.type.kind === "array") hint = elemScalarHint(rr.type.elem);
  // Pass 2: re-resolve the NON-NULL operands with the hint so a bare literal element / untyped
  // ARRAY[…] adapts. A bare NULL (pass-1 kind "null") is skipped — it must stay untyped so the
  // cat-first overload order matches PG (see the doc comment).
  if (hint !== null) {
    if (rl.type.kind !== "null") rl = resolve(scope, lhs, hint, ag, params);
    if (rr.type.kind !== "null") rr = resolve(scope, rhs, hint, ag, params);
  }
  // Try the three concat overloads in catalog order; the first whose slots unify wins.
  const tys: ResolvedType[] = [rl.type, rr.type];
  let chosen: { argFamilies: readonly string[]; result: string } | undefined;
  let elem: ResolvedType | null = null;
  for (const o of OPERATORS) {
    if (o.kind !== "concat") continue;
    const m = matchPoly(o.argFamilies, tys);
    if (m.matched) {
      chosen = o;
      elem = m.elem;
      break;
    }
  }
  if (!chosen) throw noOverload();
  const type = polyResultType(chosen.result, elem);
  // The matched overload's slot pattern selects the kernel; the operands stay in source order
  // (array_prepend's kernel already reads vals[0]=element, vals[1]=array).
  let func: ArrayFuncName;
  if (chosen.argFamilies[0] === "anyarray" && chosen.argFamilies[1] === "anyarray")
    func = "array_cat";
  else if (chosen.argFamilies[0] === "anyarray" && chosen.argFamilies[1] === "anyelement")
    func = "array_append";
  else func = "array_prepend";
  return { node: { kind: "arrayFunc", func, args: [rl.node, rr.node] }, type };
}

// noSetOpOverload is the "operator does not exist" error (42883) for a containment/positional
// operator whose operands are neither arrays of a common element type nor ranges of a common element
// type (matches PG).
export function noSetOpOverload(): EngineError {
  return engineError(
    "undefined_function",
    "operator does not exist: the operands are not arrays or ranges of a common element type",
  );
}

// resolveSetOp resolves a containment / overlap / positional operator (`@>` `<@` `&&` `<<` `>>` `&<`
// `&>` `-|-`), choosing the axis by operand type: an array operand → the array containment surface
// (array-functions.md §10, only `@>`/`<@`/`&&`); a range operand → the range boolean surface
// (range-functions.md §3). The result is always boolean (strict — a NULL operand short-circuits to
// NULL at eval). A non-array / non-range pair, or a positional operator on arrays, is 42883.
export function resolveSetOp(
  scope: Scope,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  // Pass 1: resolve both operands with no hint.
  let rl = resolve(scope, lhs, null, ag, params);
  let rr = resolve(scope, rhs, null, ag, params);
  // RANGE axis if either operand is a range. (The five positional operators are range-only; on a
  // non-range pair they fall through to the array branch below, which rejects them as 42883.)
  if (rl.type.kind === "range" || rr.type.kind === "range") {
    return resolveRangeOp(scope, op, lhs, rhs, rl, rr, ag, params);
  }

  // JSONB axis: only @>/<@ have a jsonb overload (json-sql-functions.md §1, J5). A jsonb operand
  // (or a string literal adapting to one) routes here; `&&`/the positional operators have no jsonb
  // overload and fall through to the array branch (42883). A json operand has no @> opclass (42883).
  if (
    (op === "contains" || op === "containedBy") &&
    (rl.type.kind === "jsonb" || rr.type.kind === "jsonb")
  ) {
    return resolveJsonbContains(scope, op, lhs, rhs, ag, params);
  }

  // ARRAY axis: only @>/<@/&& have an array overload (array-functions.md §10).
  let func: ArrayFuncName;
  if (op === "contains") func = "contains";
  else if (op === "containedBy") func = "contained_by";
  else if (op === "overlaps") func = "overlaps";
  // A positional/adjacency operator on non-range operands — no array overload exists.
  else throw noSetOpOverload();

  // The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8).
  let hint: ScalarType | null = null;
  if (rl.type.kind === "array") hint = elemScalarHint(rl.type.elem);
  else if (rr.type.kind === "array") hint = elemScalarHint(rr.type.elem);
  // Pass 2: re-resolve the NON-NULL operands with the hint so a bare ARRAY[…] adapts. A bare NULL
  // (pass-1 kind "null") is left untyped — it defers in the anyarray slot, result is boolean anyway.
  if (hint !== null) {
    if (rl.type.kind !== "null") rl = resolve(scope, lhs, hint, ag, params);
    if (rr.type.kind !== "null") rr = resolve(scope, rhs, hint, ag, params);
  }
  // Both slots are anyarray: the element types must unify (a non-array / mismatch is 42883).
  const tys: ResolvedType[] = [rl.type, rr.type];
  if (!matchPoly(["anyarray", "anyarray"], tys).matched) throw noSetOpOverload();
  return {
    node: { kind: "arrayFunc", func, args: [rl.node, rr.node] },
    type: { kind: "bool" },
  };
}

// resolveJsonAccess resolves a jsonb accessor operator (`-> ->> #> #>>`,
// spec/design/json-sql-functions.md §1). The base must be `jsonb` (a `json` base is the deferred
// 0A000 follow-on — json.md §4; any other base is 42883). For `->`/`->>` the argument is a key
// (`text`) or an array index (`integer`); for `#>`/`#>>` it is a `text[]` path (a bare string
// literal `'{a,b}'` adapts via array_in). The result is `jsonb` (`-> #>`) or `text` (`->> #>>`); a
// missing access yields SQL NULL at eval.
export function resolveJsonAccess(
  scope: Scope,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const rbase = resolve(scope, lhs, null, ag, params);
  // The base must be jsonb. json is a documented deferred follow-on (its operators preserve the
  // verbatim sub-text — json.md §4); any other base type has no such operator (42883).
  switch (rbase.type.kind) {
    case "jsonb":
      break;
    case "json":
      throw engineError(
        "feature_not_supported",
        "json accessor operators are not supported yet; cast to jsonb",
      );
    case "null":
      break; // a NULL base propagates (the access is NULL)
    default:
      throw engineError(
        "undefined_function",
        `operator does not exist: ${rtName(rbase.type)} ${jsonOpSymbol(op)} ...`,
      );
  }
  let jop: JsonGetOp;
  let result: ResolvedType;
  let path: boolean;
  switch (op) {
    case "jsonGet":
      jop = "arrow";
      result = { kind: "jsonb" };
      path = false;
      break;
    case "jsonGetText":
      jop = "arrowText";
      result = { kind: "text" };
      path = false;
      break;
    case "jsonGetPath":
      jop = "hashArrow";
      result = { kind: "jsonb" };
      path = true;
      break;
    default: // "jsonGetPathText"
      jop = "hashArrowText";
      result = { kind: "text" };
      path = true;
      break;
  }
  let rarg: RExpr;
  if (path) {
    // `#>` / `#>>` take a text[] path. A bare string literal `'{a,b}'` adapts via array_in;
    // otherwise the resolved argument must be a text[] (else 42883).
    if (rhs.kind === "literal" && rhs.literal.kind === "text") {
      const val = coerceStringToArray(rhs.literal.text, { kind: "scalar", scalar: "text" });
      rarg = valueToRExpr(val);
    } else {
      const ra = resolve(scope, rhs, null, ag, params);
      const argTy = ra.type;
      if (!(argTy.kind === "array" && argTy.elem.kind === "text") && argTy.kind !== "null") {
        throw engineError("undefined_function", "the #> / #>> path argument must be text[]");
      }
      rarg = ra.node;
    }
  } else {
    // `->` / `->>` take a key (text) or an array index (integer). A string literal stays text;
    // an integer literal stays integer; no adaptation is needed.
    const ra = resolve(scope, rhs, null, ag, params);
    const argTy = ra.type;
    if (argTy.kind !== "text" && argTy.kind !== "int" && argTy.kind !== "null") {
      throw engineError(
        "undefined_function",
        `operator does not exist: jsonb ${jsonOpSymbol(op)} ${rtName(argTy)}`,
      );
    }
    rarg = ra.node;
  }
  return { node: { kind: "jsonGet", op: jop, base: rbase.node, arg: rarg }, type: result };
}

// jsonOpSymbol is the display symbol for a jsonb accessor operator, for error messages.
export function jsonOpSymbol(op: BinaryOp): string {
  switch (op) {
    case "jsonGet":
      return "->";
    case "jsonGetText":
      return "->>";
    case "jsonGetPath":
      return "#>";
    case "jsonGetPathText":
      return "#>>";
    default:
      return "?";
  }
}

// jsonArgNode is the node tree of a json/jsonb function argument: a jsonb value IS the canonical
// node; a json value is parsed from its verbatim text on demand, preserving key order + duplicates
// (json.md §4). The resolver restricts a json/jsonb function argument to json/jsonb.
export function jsonArgNode(v: Value): JsonNode {
  if (v.kind === "jsonb") return v.node;
  if (v.kind === "json") return parsePreservingJson(v.text);
  throw new Error("jsonArgNode: a json/jsonb function argument must be json/jsonb");
}

// evalJsonpath recompiles a `jsonpath` value's canonical text and evaluates it over a `jsonb` context
// value (the shared kernel of the jsonpath query functions). A NULL context or path yields `null`
// (→ SQL NULL / zero rows). Port of impl/rust/src/executor.rs `eval_jsonpath`.
export function evalJsonpath(ctx: Value, path: Value): JsonNode[] | null {
  if (ctx.kind === "null" || path.kind === "null") return null;
  const node = jsonArgNode(ctx);
  if (path.kind !== "jsonpath") {
    throw new Error("resolver restricts a jsonpath argument to jsonpath");
  }
  return jsonPathEval(jsonPathCompile(path.text), node);
}

// jsonPredKindMatches reports whether a parsed JSON node matches an `IS JSON [kind]` predicate's kind
// (json-sql-functions.md §5).
export function jsonPredKindMatches(node: JsonNode, kind: JsonPredicateKind): boolean {
  switch (kind) {
    case "value":
      return true;
    case "scalar":
      return node.kind !== "object" && node.kind !== "array";
    case "array":
      return node.kind === "array";
    case "object":
      return node.kind === "object";
  }
}

// valueToNode is the JSON image of any value — the to_jsonb kernel (json-sql-functions.md §2), also
// reused by the json aggregates (B4). Numbers stay exact (decimal, never float); a json/jsonb value
// canonicalizes; a 1-D array maps to a JSON array recursively (a NULL element → JSON null). The
// type-info-dependent / float-divergent sources — composite (needs field names), float (the
// binary→decimal divergence), datetime/uuid/bytea/interval (string-render divergences), and a
// multidimensional array — are a deferred 0A000 follow-on.
export function valueToNode(v: Value): JsonNode {
  switch (v.kind) {
    case "null": // an array element (a top-level NULL is strict-propagated)
      return { kind: "null" };
    case "bool":
      return { kind: "bool", value: v.value };
    case "int":
      return { kind: "number", dec: Decimal.fromBigInt(v.int) };
    case "decimal":
      return { kind: "number", dec: v.dec };
    case "text":
      return { kind: "string", value: v.text };
    case "jsonb":
      return v.node;
    case "json":
      return jsonbIn(v.text);
    case "array": {
      if (v.dims.length > 1) {
        throw engineError(
          "feature_not_supported",
          "to_jsonb of a multidimensional array is not supported yet",
        );
      }
      const elements = v.elements.map((e) => valueToNode(e));
      return { kind: "array", elements };
    }
    case "f32":
    case "f64":
      throw engineError("feature_not_supported", "to_jsonb of a float value is not supported yet");
    case "composite":
      throw engineError(
        "feature_not_supported",
        "to_jsonb of a composite value is not supported yet",
      );
    case "uuid":
    case "date":
    case "timestamp":
    case "timestamptz":
    case "interval":
    case "bytea":
      throw engineError("feature_not_supported", "to_jsonb of this type is not supported yet");
    case "range":
      throw engineError("feature_not_supported", "to_jsonb of a range value is not supported yet");
    case "jsonpath":
      throw engineError(
        "feature_not_supported",
        "to_jsonb of a jsonpath value is not supported yet",
      );
    default: // unfetched
      throw new Error("BUG: unfetched large value escaped the storage layer");
  }
}

// elemJsonText is one element's `json`-builder text image (json-sql-functions.md §2): a `json` value
// embeds VERBATIM, a `jsonb` value its canonical (spaced) render, everything else the compact
// to_jsonb image (valueToNode → jsonCompactOut). This is how PG's json_build_array/json_build_object
// (and to_json) embed an argument's own json form.
export function elemJsonText(v: Value): string {
  if (v.kind === "json") return v.text;
  if (v.kind === "jsonb") return jsonbOut(v.node);
  return jsonCompactOut(valueToNode(v));
}

// objectKeyText is the text form of a `json[b]_build_object` KEY argument (1-based `pos` for the
// error message). PG coerces a key to text via the type's output: text as-is, integer/decimal/boolean
// rendered. A NULL key is `22023`; a non-scalar key type is a deferred `0A000` follow-on. jed integers
// are bigint, rendered via toString (never a float path — CLAUDE.md §2).
export function objectKeyText(v: Value, pos: number): string {
  switch (v.kind) {
    case "null":
      throw engineError("invalid_parameter_value", `argument ${pos}: key must not be null`);
    case "text":
      return v.text;
    case "int":
      return v.int.toString();
    case "decimal":
      return v.dec.render();
    case "bool":
      return v.value ? "true" : "false";
    default:
      throw engineError(
        "feature_not_supported",
        "a json_build_object key of this type is not supported yet",
      );
  }
}

// objectKeyNull is the `22004` raised when a `json_object` / `jsonb_object` key element is NULL.
export function objectKeyNull(): EngineError {
  return engineError("null_value_not_allowed", "null value not allowed for object key");
}

// valueToOptTextArray extracts a `text[]` value into a list of (string | null), preserving NULL
// elements (null for a NULL element). Used by `json_object` (a NULL value → JSON null; a NULL key →
// 22004). The resolver guarantees a `text[]` argument, so non-text elements cannot occur.
export function valueToOptTextArray(v: Value): (string | null)[] {
  if (v.kind !== "array") throw new Error("resolver guarantees a text[] arg");
  return v.elements.map((e) => (e.kind === "text" ? e.text : null));
}

// resolveJsonbContains resolves a jsonb containment operator `@>` / `<@` (json-sql-functions.md §1,
// J5). Both operands must be `jsonb` (a bare string literal adapts via `jsonbIn`); a `json` operand
// has no @> operator class (42883). `<@` resolves to a "jsonContains" node with the operands swapped
// (`a <@ b` is `b @> a`). The result is boolean; the operator is strict (a NULL operand → SQL NULL).
export function resolveJsonbContains(
  scope: Scope,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  // Resolve each operand with a jsonb context, so a bare `'{"a":1}'` string literal adapts.
  const resolveJsonb = (e: Expr): RExpr => {
    const { node, type } = resolve(scope, e, "jsonb", ag, params);
    if (type.kind === "jsonb" || type.kind === "null") return node;
    throw engineError(
      "undefined_function",
      `operator does not exist: ${rtName(type)} ${binaryOpSymbol(op)} jsonb`,
    );
  };
  const rl = resolveJsonb(lhs);
  const rr = resolveJsonb(rhs);
  // `a @> b` keeps the order; `a <@ b` is `b @> a`.
  const [a, b] = op === "containedBy" ? [rr, rl] : [rl, rr];
  return { node: { kind: "jsonContains", a, b }, type: { kind: "bool" } };
}

// resolveJsonHasKey resolves a jsonb key-existence operator `?` / `?|` / `?&` (json-sql-functions.md
// §1, J5). The base must be `jsonb` (a json base is 42883 — no operator). `?` takes a `text` key;
// `?|`/`?&` take a `text[]` (a bare `'{a,b}'` string literal adapts via array_in). The result is
// boolean; the operator is strict.
export function resolveJsonHasKey(
  scope: Scope,
  kind: HasKeyKind,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const rbase = resolve(scope, lhs, "jsonb", ag, params);
  if (rbase.type.kind !== "jsonb" && rbase.type.kind !== "null") {
    throw engineError(
      "undefined_function",
      `operator does not exist: ${rtName(rbase.type)} ${hasKeySymbol(kind)}`,
    );
  }
  let rarg: RExpr;
  if (kind === "one") {
    // `?` takes a single text key.
    const ra = resolve(scope, rhs, "text", ag, params);
    if (ra.type.kind !== "text" && ra.type.kind !== "null") {
      throw engineError("undefined_function", "the ? operator's right argument must be text");
    }
    rarg = ra.node;
  } else {
    // `?|` / `?&` take a text[] (a bare string literal adapts via array_in).
    if (rhs.kind === "literal" && rhs.literal.kind === "text") {
      const val = coerceStringToArray(rhs.literal.text, { kind: "scalar", scalar: "text" });
      rarg = valueToRExpr(val);
    } else {
      const ra = resolve(scope, rhs, null, ag, params);
      const argTy = ra.type;
      if (!(argTy.kind === "array" && argTy.elem.kind === "text") && argTy.kind !== "null") {
        throw engineError(
          "undefined_function",
          "the ?| / ?& operator's right argument must be text[]",
        );
      }
      rarg = ra.node;
    }
  }
  return {
    node: { kind: "jsonHasKey", hasKeyKind: kind, base: rbase.node, arg: rarg },
    type: { kind: "bool" },
  };
}

// hasKeySymbol is the display symbol for a key-existence operator, for error messages.
export function hasKeySymbol(kind: HasKeyKind): string {
  switch (kind) {
    case "one":
      return "?";
    case "any":
      return "?|";
    default: // "all"
      return "?&";
  }
}

// resolveJsonbConcat resolves a jsonb `||` concatenation/merge (json-sql-functions.md §1, J6). Both
// operands must be jsonb (a string literal adapts via `jsonbIn`). Result jsonb; strict.
export function resolveJsonbConcat(
  scope: Scope,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const resolveJsonb = (e: Expr): RExpr => {
    const { node, type } = resolve(scope, e, "jsonb", ag, params);
    if (type.kind === "jsonb" || type.kind === "null") return node;
    throw engineError("undefined_function", `operator does not exist: ${rtName(type)} || jsonb`);
  };
  const a = resolveJsonb(lhs);
  const b = resolveJsonb(rhs);
  return { node: { kind: "jsonConcat", a, b }, type: { kind: "jsonb" } };
}

// resolveJsonbDelete resolves a jsonb delete operator: `-` (key `text` / index `int` / keys
// `text[]`) or `#-` (path `text[]`) — json-sql-functions.md §1, J6. The base is already resolved
// (rbase, jsonb-typed). The form is chosen by the argument type; a bare `'{a,b}'` string literal
// adapts to `text[]` only for `#-` (for `-` it is a single text key, verbatim like PG). Result
// jsonb; strict.
export function resolveJsonbDelete(
  scope: Scope,
  isPath: boolean,
  rhs: Expr,
  rbase: RExpr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  let kind: DeleteKind;
  let rarg: RExpr;
  if (isPath) {
    // `#-` always takes a text[] path (a bare '{a,b}' literal adapts via array_in).
    kind = "path";
    rarg = resolveTextArrayArg(scope, rhs, "#-", ag, params);
  } else if (rhs.kind === "literal" && rhs.literal.kind === "text") {
    // A bare string literal is a text key (`jsonb - 'a'`), NOT a text[].
    const r = resolve(scope, rhs, "text", ag, params);
    kind = "key";
    rarg = r.node;
  } else {
    const r = resolve(scope, rhs, null, ag, params);
    const t = r.type;
    if (t.kind === "text" || t.kind === "null") {
      kind = "key";
      rarg = r.node;
    } else if (t.kind === "int") {
      kind = "index";
      rarg = r.node;
    } else if (t.kind === "array" && t.elem.kind === "text") {
      kind = "keys";
      rarg = r.node;
    } else {
      throw engineError(
        "undefined_function",
        `operator does not exist: jsonb - ${rtName(t)} (expected text, integer, or text[])`,
      );
    }
  }
  return {
    node: { kind: "jsonDelete", deleteKind: kind, base: rbase, arg: rarg },
    type: { kind: "jsonb" },
  };
}

// resolveTextArrayArg resolves a `text[]` operator argument (the `#-` path): a bare string literal
// `'{a,b}'` adapts via `coerceStringToArray`; otherwise the resolved type must be `text[]` (or NULL).
// `sym` is the operator symbol for the error message.
export function resolveTextArrayArg(
  scope: Scope,
  rhs: Expr,
  sym: string,
  ag: AggCtx,
  params: ParamTypes,
): RExpr {
  if (rhs.kind === "literal" && rhs.literal.kind === "text") {
    const val = coerceStringToArray(rhs.literal.text, { kind: "scalar", scalar: "text" });
    return valueToRExpr(val);
  }
  const r = resolve(scope, rhs, null, ag, params);
  const t = r.type;
  if ((t.kind === "array" && t.elem.kind === "text") || t.kind === "null") return r.node;
  throw engineError("undefined_function", `the ${sym} operator's right argument must be text[]`);
}

// resolveJsonbSetInsert resolves jsonb_set / jsonb_insert (json-sql-functions.md §2): `(target jsonb,
// path text[], value jsonb [, flag boolean])` → jsonb. A bare `'{a,b}'` path literal adapts to text[]
// and a bare string `value` literal adapts to jsonb (the `Some(ScalarType::Jsonb)` hint). STRICT (the
// eval propagates any NULL). The optional flag defaults to `true` for jsonb_set (create_if_missing) /
// `false` for jsonb_insert (insert_after).
export function resolveJsonbSetInsert(
  scope: Scope,
  name: string,
  mode: PathSetMode,
  args: Expr[],
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (args.length !== 3 && args.length !== 4) throw noFuncOverload(name);
  const target = resolve(scope, args[0]!, "jsonb", ag, params);
  if (target.type.kind !== "jsonb" && target.type.kind !== "null") throw noFuncOverload(name);
  const path = resolveTextArrayArg(scope, args[1]!, name, ag, params);
  const value = resolve(scope, args[2]!, "jsonb", ag, params);
  if (value.type.kind !== "jsonb" && value.type.kind !== "null") throw noFuncOverload(name);
  let flag: RExpr;
  if (args.length === 4) {
    const f = resolve(scope, args[3]!, "boolean", ag, params);
    if (f.type.kind !== "bool" && f.type.kind !== "null") throw noFuncOverload(name);
    flag = f.node;
  } else {
    // Default: jsonb_set create_if_missing = true; jsonb_insert insert_after = false.
    flag = { kind: "constBool", value: mode === "set" };
  }
  return {
    node: { kind: "jsonSetInsert", mode, args: [target.node, path, value.node, flag] },
    type: { kind: "jsonb" },
  };
}

// resolveJsonObject resolves `json_object` / `jsonb_object` (json-sql-functions.md §2): one `text[]`
// of alternating keys/values, or two `text[]` (keys, values). A bare `'{…}'` literal adapts to text[].
// STRICT (the eval propagates a NULL whole-array argument). Wrong arity (not 1 or 2 args) → 42883.
export function resolveJsonObject(
  scope: Scope,
  name: string,
  json: boolean,
  args: Expr[],
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (args.length === 0 || args.length > 2) throw noFuncOverload(name);
  const rargs: RExpr[] = [];
  for (const a of args) rargs.push(resolveTextArrayArg(scope, a, name, ag, params));
  return {
    node: { kind: "jsonObjectFromArrays", json, args: rargs },
    type: { kind: json ? "json" : "jsonb" },
  };
}

// resolveJsonpathFn resolves a scalar jsonpath query function (P2, jsonpath.md §5): `(ctx jsonb, path
// jsonpath)`. A bare string literal adapts (the context to jsonb, the path to a compiled jsonpath).
// STRICT.
export function resolveJsonpathFn(
  scope: Scope,
  name: string,
  kind: JsonPathFnKind,
  args: Expr[],
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const [ctx, path] = resolveJsonpathArgs(scope, name, args, ag, params);
  const type: ResolvedType =
    kind === "exists" || kind === "match" ? { kind: "bool" } : { kind: "jsonb" };
  return { node: { kind: "jsonPathFn", pathFnKind: kind, args: [ctx, path] }, type };
}

// resolveJsonpathArgs resolves the `(context jsonb, path jsonpath)` argument pair shared by the
// jsonpath query functions (the SRF and the scalar forms). A bare string literal adapts: the context
// to jsonb, the path to a compiled `jsonpath`. Exactly two args this slice (the optional `vars` /
// `silent` are a follow-on).
export function resolveJsonpathArgs(
  scope: Scope,
  name: string,
  args: Expr[],
  ag: AggCtx,
  params: ParamTypes,
): [RExpr, RExpr] {
  if (args.length !== 2) throw noFuncOverload(name);
  const ctx = resolve(scope, args[0]!, "jsonb", ag, params);
  if (ctx.type.kind !== "jsonb" && ctx.type.kind !== "null") throw noFuncOverload(name);
  const path = resolve(scope, args[1]!, "jsonpath", ag, params);
  if (path.type.kind !== "jsonpath" && path.type.kind !== "null") throw noFuncOverload(name);
  return [ctx.node, path.node];
}

// resolveJsonSqlFn resolves a SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY
// (json-sql-functions.md §5, S2) → a "jsonSqlFn" RExpr + its fixed result type. Port of
// impl/rust/src/executor.rs `resolve_json_sql_fn`.
export function resolveJsonSqlFn(
  scope: Scope,
  kind: JsonSqlKind,
  ctx: Expr,
  path: Expr,
  returning: string | null,
  wrapper: JsonWrapper,
  keepQuotes: boolean,
  onEmpty: JsonOnBehavior | null,
  onError: JsonOnBehavior | null,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  // The context item — json / jsonb / text, coerced to a jsonb document at eval; a bare string
  // literal adapts to jsonb.
  const rctx = resolve(scope, ctx, "jsonb", ag, params);
  switch (rctx.type.kind) {
    case "jsonb":
    case "json":
    case "text":
    case "null":
      break;
    default:
      throw engineError(
        "datatype_mismatch",
        "the context item of a SQL/JSON query function must be json/jsonb/text, not " +
          rtName(rctx.type),
      );
  }
  // The path — a jsonpath; a bare string literal compiles.
  const rpath = resolve(scope, path, "jsonpath", ag, params);
  if (rpath.type.kind !== "jsonpath" && rpath.type.kind !== "null") {
    throw engineError(
      "datatype_mismatch",
      "the path of a SQL/JSON query function must be a jsonpath",
    );
  }
  // OMIT QUOTES is the deferred S2 follow-on (the jsonb-of-bare-text result quirk).
  if (!keepQuotes) {
    throw engineError("feature_not_supported", "JSON_QUERY OMIT QUOTES is not supported yet");
  }
  // The fixed RETURNING scalar type.
  let returningSt: ScalarType;
  if (kind === "exists") {
    returningSt = "boolean";
  } else if (returning === null) {
    returningSt = kind === "value" ? "text" : "jsonb";
  } else {
    const st = scalarTypeFromName(returning);
    if (st === undefined) {
      throw engineError("undefined_object", `type "${returning}" does not exist`);
    }
    returningSt = st;
  }
  // JSON_QUERY's result must be a JSON type (json/jsonb); JSON_VALUE's must be a scalar — a
  // composite/array RETURNING is a deferred 0A000 (it cannot hold an extracted scalar).
  if (kind === "query" && returningSt !== "json" && returningSt !== "jsonb") {
    throw engineError(
      "feature_not_supported",
      "JSON_QUERY RETURNING a non-json type is not supported yet",
    );
  }
  const onEmptyB: JsonOnBehavior = onEmpty ?? "null";
  const onErrorB: JsonOnBehavior = onError ?? (kind === "exists" ? "false" : "null");
  return {
    node: {
      kind: "jsonSqlFn",
      sqlKind: kind,
      args: [rctx.node, rpath.node],
      returning: returningSt,
      decimal: null,
      wrapper,
      keepQuotes,
      onEmpty: onEmptyB,
      onError: onErrorB,
    },
    type: resolvedTypeOf(returningSt),
  };
}

// rangeOpFor maps a containment/positional BinaryOp to its range-against-range kernel (RangeOpName).
export function rangeOpFor(op: BinaryOp): RangeOpName {
  switch (op) {
    case "contains":
      return "contains";
    case "containedBy":
      return "containedBy";
    case "overlaps":
      return "overlaps";
    case "strictlyLeft":
      return "before";
    case "strictlyRight":
      return "after";
    case "notExtendRight":
      return "overleft";
    case "notExtendLeft":
      return "overright";
    case "adjacent":
      return "adjacent";
    default:
      throw new Error("rangeOpFor is only called for the eight set/positional operators");
  }
}

// resolveRangeOp resolves the RANGE axis of a containment/positional operator (range-functions.md §3),
// with both operands already resolved (pass 1, to avoid double aggregate collection — only the element
// operand re-resolves with the element hint). The overload is chosen by the operand types: range×range
// (the elements must match, else 42883) for every operator; the bare element overloads `range @>
// element` and `element <@ range` re-resolve the element operand with the range's element type as the
// hint and type-check assignability. A bare untyped NULL on one side is treated as a NULL range (the
// range×range overload; eval yields NULL). Anything else is 42883.
export function resolveRangeOp(
  scope: Scope,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
  rl: { node: RExpr; type: ResolvedType },
  rr: { node: RExpr; type: ResolvedType },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const lt = rl.type;
  const rt = rr.type;
  // range × range: the elements must match.
  if (lt.kind === "range" && rt.kind === "range") {
    const le = resolvedRangeElementScalar(lt.elem);
    const re = resolvedRangeElementScalar(rt.elem);
    if (le === undefined || re === undefined || le !== re) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: rangeOpFor(op),
        args: [rl.node, rr.node],
        elem: le,
      },
      type: { kind: "bool" },
    };
  }
  // range × NULL (a bare NULL is taken as a NULL range; eval yields NULL).
  if (lt.kind === "range" && rt.kind === "null") {
    const le = resolvedRangeElementScalar(lt.elem);
    if (le === undefined) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: rangeOpFor(op),
        args: [rl.node, rr.node],
        elem: le,
      },
      type: { kind: "bool" },
    };
  }
  if (lt.kind === "null" && rt.kind === "range") {
    const re = resolvedRangeElementScalar(rt.elem);
    if (re === undefined) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: rangeOpFor(op),
        args: [rl.node, rr.node],
        elem: re,
      },
      type: { kind: "bool" },
    };
  }
  // `range @> element` — the element overload of `@>` (the only operator with one). Re-resolve the
  // right operand with the range's element as the hint, then check it is assignable.
  if (lt.kind === "range" && op === "contains") {
    const elem = resolvedRangeElementScalar(lt.elem);
    if (elem === undefined) throw noSetOpOverload();
    const re = resolve(scope, rhs, elem, ag, params);
    if (!rangeBoundAssignable(re.type, elem)) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: "containsElem",
        args: [rl.node, re.node],
        elem,
      },
      type: { kind: "bool" },
    };
  }
  // `element <@ range` — the element overload of `<@`.
  if (rt.kind === "range" && op === "containedBy") {
    const elem = resolvedRangeElementScalar(rt.elem);
    if (elem === undefined) throw noSetOpOverload();
    const le = resolve(scope, lhs, elem, ag, params);
    if (!rangeBoundAssignable(le.type, elem)) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: "elemContainedBy",
        args: [le.node, rr.node],
        elem,
      },
      type: { kind: "bool" },
    };
  }
  throw noSetOpOverload();
}

// resolveRangeSetOp resolves a range SET operator (`+` union, `-` difference, `*` intersection —
// range-functions.md §4), reached from resolveBinary when a `+`/`-`/`*` has a range operand (the
// operands are already resolved). Both must be ranges over the SAME element type — a range × non-range,
// or a cross-element pair, is 42883 (PG's "operator does not exist"); a bare untyped NULL beside a range
// is taken as a NULL range (the range×range overload; eval → NULL, strict). The result is a range over
// that element type. range_merge does NOT come through here (it is a function call — see
// resolveRangeFunc); it shares the "rangeSetOp" node with op = "merge".
export function resolveRangeSetOp(
  op: BinaryOp,
  rl: RExpr,
  lt: ResolvedType,
  rr: RExpr,
  rt: ResolvedType,
): { node: RExpr; type: ResolvedType } {
  let elem: ScalarType;
  if (lt.kind === "range" && rt.kind === "range") {
    const le = resolvedRangeElementScalar(lt.elem);
    const re = resolvedRangeElementScalar(rt.elem);
    if (le === undefined || re === undefined || le !== re) throw noSetOpOverload();
    elem = le;
  } else if (lt.kind === "range" && rt.kind === "null") {
    const le = resolvedRangeElementScalar(lt.elem);
    if (le === undefined) throw noSetOpOverload();
    elem = le;
  } else if (lt.kind === "null" && rt.kind === "range") {
    const re = resolvedRangeElementScalar(rt.elem);
    if (re === undefined) throw noSetOpOverload();
    elem = re;
  } else {
    // A range paired with a non-range (or any other combination) — no such operator.
    throw noSetOpOverload();
  }
  let setop: RangeSetOpName;
  if (op === "add") setop = "union";
  else if (op === "sub") setop = "difference";
  else if (op === "mul") setop = "intersect";
  else throw new Error("resolveRangeSetOp is only called for +, -, *");
  return {
    node: { kind: "rangeSetOp", op: setop, args: [rl, rr] },
    type: { kind: "range", elem: resolvedTypeOf(elem) },
  };
}

// resolveQuantified resolves a quantified array comparison `x op ANY/SOME/ALL(arr)`
// (array-functions.md §11): the array spelling of IN. `x` (lhs) and the array operand resolve with
// the SAME literal adaptation the comparison operators use — a bare-literal `x` adapts to the array's
// element type, a bare ARRAY[…] operand adapts its elements to `x`'s type. The right operand must be
// an array (a non-array side is 42809; a bare untyped NULL is 42P18); `x` and the element type must
// be comparable (else 42883, PG's operator-not-found). The result is always boolean.
export function resolveQuantified(
  scope: Scope,
  op: BinaryOp,
  all: boolean,
  lhs: Expr,
  array: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  // Pass 1: resolve both operands with no hint.
  let rl = resolve(scope, lhs, null, ag, params);
  let ra = resolve(scope, array, null, ag, params);
  // If `x` is a CONCRETE scalar (not itself an adaptable bare literal) and the array operand is a
  // bare ARRAY[…] constructor, re-resolve the array with `x`'s type as the element hint so the
  // constructor adapts (`c = ANY(ARRAY[1,2])` over an i32 column → i32[]). Harmless for a
  // column / cast operand (it ignores the hint).
  if (!isAdaptableOperand(lhs)) {
    const h = ctxOf(rl.type);
    if (h !== null) ra = resolve(scope, array, h, ag, params);
  }
  // If the array resolved to E[] and `x` is an adaptable bare literal, adapt `x` to E (with a range
  // check) — exactly the operand pairing `=` uses (`5 = ANY(i32[]_col)` lands `x` on i32).
  if (ra.type.kind === "array" && isAdaptableOperand(lhs)) {
    const h = elemScalarHint(ra.type.elem);
    if (h !== null) rl = resolve(scope, lhs, h, ag, params);
  }
  // The right operand must be an array.
  if (ra.type.kind === "null") {
    // A bare untyped NULL leaves the array type undeterminable — jed's polymorphic posture (§11; the
    // unnest(NULL) / §5 #6 precedent), a documented degenerate divergence from PG.
    throw engineError(
      "indeterminate_datatype",
      "could not determine the array element type of a NULL ANY/ALL operand",
    );
  }
  if (ra.type.kind !== "array") {
    throw engineError("wrong_object_type", "op ANY/ALL (array) requires array on right side");
  }
  const elem = ra.type.elem;
  // `x` and the element type must be comparable; PG reports operator-not-found (42883) here, NOT the
  // bare 42804 a plain `int = text` raises — matching AF4's element-mismatch posture (§10.2).
  try {
    classifyComparable(rl.type, elem);
  } catch {
    throw engineError(
      "undefined_function",
      `operator does not exist: ${rtName(rl.type)} ${binaryOpSymbol(op)} ${rtName(elem)}`,
    );
  }
  return {
    node: { kind: "quantified", op, all, lhs: rl.node, array: ra.node },
    type: { kind: "bool" },
  };
}

// binaryOpSymbol is the infix symbol of a comparison/arithmetic operator, for an
// `operator does not exist` message (only the comparison operators reach resolveQuantified).
export function binaryOpSymbol(op: BinaryOp): string {
  switch (op) {
    case "eq":
      return "=";
    case "ne":
      return "<>";
    case "lt":
      return "<";
    case "gt":
      return ">";
    case "le":
      return "<=";
    case "ge":
      return ">=";
    case "add":
      return "+";
    case "sub":
      return "-";
    case "mul":
      return "*";
    case "div":
      return "/";
    case "mod":
      return "%";
    case "and":
      return "AND";
    case "or":
      return "OR";
    case "concat":
      return "||";
    case "contains":
      return "@>";
    case "containedBy":
      return "<@";
    case "overlaps":
      return "&&";
    case "strictlyLeft":
      return "<<";
    case "strictlyRight":
      return ">>";
    case "notExtendRight":
      return "&<";
    case "notExtendLeft":
      return "&>";
    case "adjacent":
      return "-|-";
    case "jsonGet":
      return "->";
    case "jsonGetText":
      return "->>";
    case "jsonGetPath":
      return "#>";
    case "jsonGetPathText":
      return "#>>";
    case "jsonHasKey":
      return "?";
    case "jsonHasAnyKey":
      return "?|";
    case "jsonHasAllKeys":
      return "?&";
    case "jsonDeletePath":
      return "#-";
    case "jsonPathExists":
      return "@?";
    case "jsonPathMatch":
      return "@@";
  }
}

// resolveOperandPair resolves the two operands of a binary operator, giving a bare
// *integer* literal the other operand's integer type as context (so `small + 1` types `1`
// as i16, and `small + 100000` traps 22003 at resolve). A text literal needs no context
// (it is always text); when the sibling is text, an integer literal gets no integer context
// (intTypeOf returns null) and defaults to i64 — the caller's family check then reports
// the mismatch. This does NOT enforce a family — resolveIntPair (arithmetic) and
// classifyComparable (comparison) layer that on top.
// resolveIntOrDecimalPair resolves a two-numeric scalar function (gcd/lcm) by reusing the arithmetic
// operand-pair resolution (literal adaptation), then settling the result type. Both operands must be
// integer or decimal (a float/other operand → 42883); the result is the promoted integer type when
// both are integer, else "decimal" (an integer operand promotes, as PG does).
export function resolveIntOrDecimalPair(
  scope: Scope,
  name: string,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { args: RExpr[]; result: ScalarType } {
  const p = resolveOperandPair(scope, lhs, rhs, ag, params);
  const ok = (t: ResolvedType) => t.kind === "int" || t.kind === "decimal" || t.kind === "null";
  if (!ok(p.lt) || !ok(p.rt)) throw noFuncOverload(name);
  const result: ScalarType =
    p.lt.kind === "decimal" || p.rt.kind === "decimal" ? "decimal" : promote(p.lt, p.rt);
  return { args: [p.rl, p.rr], result };
}

// gcdBigint is the gcd of two bigints by the Euclidean algorithm, NON-NEGATIVE (bigint is exact, so
// no intermediate overflow). The caller range-checks the result against the promoted integer type
// (so |MinInt64| = 2^63 → 22003, matching the other cores' i64::MIN-abs overflow).
export function gcdBigint(a: bigint, b: bigint): bigint {
  while (b !== 0n) {
    [a, b] = [b, a % b];
  }
  return a < 0n ? -a : a;
}

// gcdDecimalValue is the gcd of two decimals by the Euclidean algorithm over rem, NON-NEGATIVE at
// scale max(sₐ, s_b) (PG numeric gcd). The values share a fixed scale through the chain, so it
// reduces to an integer gcd and terminates; the final pad to the target scale is exact.
export function gcdDecimalValue(a: Decimal, b: Decimal): Decimal {
  const target = Math.max(a.scale, b.scale);
  let x = a;
  let y = b;
  while (!y.isZero()) {
    const r = x.rem(y);
    [x, y] = [y, r];
  }
  return x.abs().roundToScale(target);
}

// lcmBigint is the lcm of two bigints, NON-NEGATIVE: |a/gcd·b| (bigint is exact). The caller
// range-checks the result against the promoted integer type (an out-of-range magnitude → 22003,
// matching the other cores' checked-overflow). lcm(_, 0) = 0.
export function lcmBigint(a: bigint, b: bigint): bigint {
  if (a === 0n || b === 0n) return 0n;
  const g = gcdBigint(a, b);
  const prod = (a / g) * b;
  return prod < 0n ? -prod : prod;
}

// lcmDecimalValue is the lcm of two decimals, NON-NEGATIVE at scale max(sₐ, s_b): |a/gcd·b| (the
// a/gcd division is exact). lcm(_, 0) = 0. A magnitude over the decimal value cap traps 22003.
export function lcmDecimalValue(a: Decimal, b: Decimal): Decimal {
  const target = Math.max(a.scale, b.scale);
  if (a.isZero() || b.isZero()) return Decimal.zero(target);
  const g = gcdDecimalValue(a, b);
  return a.div(g).mul(b).abs().roundToScale(target);
}

// widthBucketErr is the 2201G raised by width_bucket for a bad count / equal-or-nonfinite bounds.
export function widthBucketErr(detail: string): EngineError {
  return engineError("invalid_argument_for_width_bucket_function", detail);
}

// minScaleOf is the minimum scale that represents d exactly — its display scale minus trailing
// fractional zeros (decimal.md, the shared engine of min_scale/trim_scale). roundToScale(t-1)
// equals the value iff the digit at scale t is zero (otherwise it rounds, changing the value), so
// the loop stops at the first non-zero fractional digit. Zero → 0.
export function minScaleOf(d: Decimal): number {
  if (d.isZero()) return 0;
  let t = d.scale;
  while (t > 0 && d.roundToScale(t - 1).cmpValue(d) === 0) t--;
  return t;
}
