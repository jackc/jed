// evalArrayFunc evaluates an array function over its already-evaluated argument values
// (spec/design/array-functions.md §3). The introspectors propagate NULL and return NULL for an
// out-of-shape request; the builders are non-strict (a NULL array argument is the identity/empty, NOT
// a propagated NULL). The resolver guarantees the array operand is an array or NULL.
import type {
  ArrayFuncName,
  EvalEnv,
  RExpr,
  RSubscript,
  RangeFuncName,
  RangeOpName,
  RangeSetOpName,
  ResolvedType,
} from "./executor.ts";
import type { Value } from "./value.ts";
import {
  arrayNdim,
  arrayUbound,
  arrayValue,
  boolValue,
  decimalValue,
  emptyArray,
  float64Value,
  intValue,
  jsonValue,
  nullValue,
  textValue,
} from "./value.ts";
import { jsonCompactOut } from "./json.ts";
import { arraySubscriptErr, distinctRowKey, promote, rtName, valueToNode } from "./executor.ts";
import type { DecimalTypmod, ScalarType } from "./types.ts";
import {
  finalizeRange,
  parseBoundFlags,
  rangeAdjacent,
  rangeAfter,
  rangeBefore,
  rangeContains,
  rangeContainsElem,
  rangeForElement,
  rangeIntersect,
  rangeMinus,
  rangeOverlaps,
  rangeOverleft,
  rangeOverright,
  rangeUnion,
} from "./range.ts";
import { engineError } from "./errors.ts";
import { storeValue, typeError } from "./store.ts";
import { valueCmp } from "./window.ts";
import type { Row } from "./storage.ts";
import { Meter } from "./cost.ts";
import { evalExpr } from "./eval.ts";
import {
  canonicalName,
  isBool,
  isBytea,
  isDate,
  isDecimal,
  isFloat,
  isInteger,
  isInterval,
  isText,
  isTimestamp,
  isTimestamptz,
  isUuid,
  promoteFloat,
  scalarTypeFromName,
} from "./types.ts";
import { Decimal, MAX_PRECISION, MAX_SCALE } from "./decimal.ts";
import type { Expr, OrderKey, SetOpKind, TypeMod } from "./ast.ts";
export function evalArrayFunc(func: ArrayFuncName, vals: Value[]): Value {
  switch (func) {
    case "array_ndims": {
      const a = vals[0]!;
      if (a.kind !== "array") return nullValue();
      return arrayNdim(a) === 0 ? nullValue() : intValue(BigInt(arrayNdim(a))); // empty → NULL (PG)
    }
    case "cardinality": {
      const a = vals[0]!;
      if (a.kind !== "array") return nullValue();
      return intValue(BigInt(a.elements.length)); // 0 for empty (NOT NULL)
    }
    case "array_dims": {
      const a = vals[0]!;
      if (a.kind !== "array" || arrayNdim(a) === 0) return nullValue();
      return textValue(arrayDimsText(a));
    }
    case "array_length":
    case "array_lower":
    case "array_upper": {
      const a = vals[0]!;
      const dimV = vals[1]!;
      if (a.kind !== "array" || dimV.kind === "null") return nullValue();
      const dim = (dimV as { int: bigint }).int;
      const nd = arrayNdim(a);
      if (nd === 0 || dim < 1n || dim > BigInt(nd)) return nullValue();
      const d = Number(dim) - 1;
      if (func === "array_length") return intValue(BigInt(a.dims[d]!));
      if (func === "array_lower") return intValue(BigInt(a.lbounds[d]!));
      return intValue(BigInt(arrayUbound(a, d)));
    }
    case "array_append":
      return arrayExtend(vals[0]!, vals[1]!, true);
    case "array_prepend":
      return arrayExtend(vals[1]!, vals[0]!, false);
    case "array_cat":
      return arrayCatValues(vals[0]!, vals[1]!);
    case "array_remove":
      return arrayRemoveValue(vals[0]!, vals[1]!);
    case "array_replace":
      return arrayReplaceValue(vals[0]!, vals[1]!, vals[2]!);
    case "array_position":
      return arrayPositionValue(vals[0]!, vals[1]!, vals.length > 2 ? vals[2]! : null);
    case "array_positions":
      return arrayPositionsValue(vals[0]!, vals[1]!);
    case "array_to_json": {
      // array_to_json(anyarray) → the array's compact JSON image (the to_jsonb node kernel). STRICT;
      // a multidimensional array propagates the to_jsonb 0A000.
      const a = vals[0]!;
      if (a.kind === "null") return nullValue();
      return jsonValue(jsonCompactOut(valueToNode(a)));
    }
    case "contains":
      return arrayContainsValue(vals[0]!, vals[1]!);
    case "contained_by":
      return arrayContainsValue(vals[1]!, vals[0]!);
    case "overlaps":
      return arrayOverlapsValue(vals[0]!, vals[1]!);
  }
}

// evalRangeFunc evaluates a range accessor (spec/design/range-functions.md §1, RF1). STRICT: a NULL
// range → NULL. lower/upper yield the bound value (NULL when empty or unbounded on that side); the
// _inc/_inf readers + isempty yield boolean. For the empty range every reader but isempty is
// false/NULL; for an infinite bound the _inf reader is true and the _inc reader false. The resolver
// guarantees the operand is a range or NULL.
export function evalRangeFunc(func: RangeFuncName, vals: Value[]): Value {
  const rv = vals[0]!;
  if (rv.kind === "null") return nullValue();
  if (rv.kind !== "range") throw new Error("range accessor: range operand");
  switch (func) {
    case "lower":
      return !rv.empty && rv.lower !== null ? rv.lower : nullValue();
    case "upper":
      return !rv.empty && rv.upper !== null ? rv.upper : nullValue();
    case "isempty":
      return boolValue(rv.empty);
    // For the empty range both inclusivity flags are false by the canonical invariant, so reading
    // them directly already yields PG's false; an infinite bound likewise stores _inc = false.
    case "lower_inc":
      return boolValue(rv.lowerInc);
    case "upper_inc":
      return boolValue(rv.upperInc);
    // The empty range is NOT infinite on either side (PG): guard before reading the bound.
    case "lower_inf":
      return boolValue(!rv.empty && rv.lower === null);
    case "upper_inf":
      return boolValue(!rv.empty && rv.upper === null);
  }
}

// evalRangeCtor evaluates a range constructor (spec/design/range-functions.md §2, RF2). `vals` is
// [lo, hi] or [lo, hi, bounds]. Each bound is coerced to the element `elem` assignment-style (a NULL
// bound → an infinite bound; an integer range-checks 22003; an int→decimal / text→temporal adapts),
// the bounds flags are read (default `[)`; a NULL 3-arg flags → 22000; an invalid flags string →
// 42601), and finalizeRange produces the canonical value (order-check 22000, canonicalize,
// empty-normalize).
export function evalRangeCtor(elem: ScalarType, vals: Value[]): Value {
  const desc = rangeForElement(elem);
  if (desc === undefined) throw new Error("a range constructor's elem has a range");
  const lower = coerceRangeBound(vals[0]!, elem);
  const upper = coerceRangeBound(vals[1]!, elem);
  let lowerInc: boolean;
  let upperInc: boolean;
  const flags = vals[2];
  if (flags === undefined) {
    // 2-arg form defaults to `[)`.
    lowerInc = true;
    upperInc = false;
  } else if (flags.kind === "null") {
    throw engineError("data_exception", "range constructor flags argument must not be null");
  } else if (flags.kind === "text") {
    [lowerInc, upperInc] = parseBoundFlags(flags.text);
  } else {
    throw new Error("resolver restricts the range bounds flags to text");
  }
  return finalizeRange(desc, lower, upper, lowerInc, upperInc);
}

// coerceRangeBound coerces one constructor bound value to the range element `elem`, returning null
// for a NULL bound (an infinite bound). Reuses storeValue (the INSERT/UPDATE assignment coercion):
// an integer range-checks into the element (22003), an int→decimal widens, a text→temporal parses,
// and a non-assignable value is 42804 (the resolver already screened the common 42883 cases).
export function coerceRangeBound(v: Value, elem: ScalarType): Value | null {
  const stored = storeValue(v, elem, null, null, false, "range bound");
  return stored.kind === "null" ? null : stored;
}

// expectRange extracts the range value the resolver guaranteed is a (non-NULL) range operand.
export function expectRange(v: Value): Value & { kind: "range" } {
  if (v.kind !== "range")
    throw new Error("the range-operator resolver guarantees a range operand here");
  return v;
}

// evalRangeOp evaluates a range boolean operator (range-functions.md §3, RF3) over two
// already-evaluated operand values. STRICT: a NULL operand → NULL. For the range-against-range
// operators both operands are ranges; for the element overloads (containsElem/elemContainedBy) the
// non-range operand is coerced to the range's element type `elem` (assignment-style, matching the
// resolver's hint). The boolean kernels live in range.ts.
export function evalRangeOp(op: RangeOpName, l: Value, r: Value, elem: ScalarType): Value {
  if (l.kind === "null" || r.kind === "null") return nullValue();
  let result: boolean;
  switch (op) {
    // `range @> element`: l is the range, r the element (coerced to the range's element type).
    case "containsElem": {
      const e = storeValue(r, elem, null, null, false, "range element");
      result = rangeContainsElem(expectRange(l), e);
      break;
    }
    // `element <@ range`: l is the element, r the range.
    case "elemContainedBy": {
      const e = storeValue(l, elem, null, null, false, "range element");
      result = rangeContainsElem(expectRange(r), e);
      break;
    }
    case "contains":
      result = rangeContains(expectRange(l), expectRange(r));
      break;
    case "containedBy":
      result = rangeContains(expectRange(r), expectRange(l));
      break;
    case "overlaps":
      result = rangeOverlaps(expectRange(l), expectRange(r));
      break;
    case "before":
      result = rangeBefore(expectRange(l), expectRange(r));
      break;
    case "after":
      result = rangeAfter(expectRange(l), expectRange(r));
      break;
    case "overleft":
      result = rangeOverleft(expectRange(l), expectRange(r));
      break;
    case "overright":
      result = rangeOverright(expectRange(l), expectRange(r));
      break;
    case "adjacent":
      result = rangeAdjacent(expectRange(l), expectRange(r));
      break;
  }
  return boolValue(result);
}

// evalRangeSetOp evaluates a range SET operator (range-functions.md §4, RF4) over two already-evaluated
// operand values. STRICT: a NULL operand → NULL. "union"/"difference" raise 22000 on a non-contiguous
// result; "intersect"/"merge" never error. The kernels live in range.ts.
export function evalRangeSetOp(op: RangeSetOpName, l: Value, r: Value): Value {
  if (l.kind === "null" || r.kind === "null") return nullValue();
  const a = expectRange(l);
  const b = expectRange(r);
  switch (op) {
    case "union":
      return rangeUnion(a, b, true);
    case "merge":
      return rangeUnion(a, b, false);
    case "intersect":
      return rangeIntersect(a, b);
    case "difference":
      return rangeMinus(a, b);
  }
}

// notDistinct is IS NOT DISTINCT FROM at the value level (array-functions.md §5 #10): jed's total
// element comparator, so NULL equals NULL and a non-NULL never equals NULL.
export function notDistinct(a: Value, b: Value): boolean {
  return valueCmp(a, b) === 0;
}

// strictElemEq is STRICT element equality for the containment/overlap operators (array-functions.md
// §10): a NULL element equals NOTHING — including another NULL — the deliberate inverse of notDistinct
// (§5 #10). For two non-NULL values it is jed's total element comparator (valueCmp === 0).
export function strictElemEq(a: Value, b: Value): boolean {
  return a.kind !== "null" && b.kind !== "null" && valueCmp(a, b) === 0;
}

// arrayContainsValue is a @> b (array-functions.md §10): does a CONTAIN b — is every element of b
// present in a under STRICT equality, over the flattened element multiset (any dimensionality)? A NULL
// whole-array operand → NULL. The empty array is contained by anything (a @> {} is true).
export function arrayContainsValue(a: Value, b: Value): Value {
  if (a.kind !== "array" || b.kind !== "array") return nullValue();
  const contained = b.elements.every((eb) => a.elements.some((ea) => strictElemEq(ea, eb)));
  return boolValue(contained);
}

// arrayOverlapsValue is a && b (array-functions.md §10): do a and b OVERLAP — share at least one
// element under STRICT equality, over the flattened element multiset (any dimensionality)? A NULL
// whole-array operand → NULL. The empty array overlaps nothing.
export function arrayOverlapsValue(a: Value, b: Value): Value {
  if (a.kind !== "array" || b.kind !== "array") return nullValue();
  const overlaps = a.elements.some((ea) => b.elements.some((eb) => strictElemEq(ea, eb)));
  return boolValue(overlaps);
}

// arrayRemoveValue is array_remove(a, e) (array-functions.md §8): drop every element NOT DISTINCT
// FROM e. NULL array → NULL; 1-D/empty only (a multidimensional array is 0A000); the lower bound is
// preserved and an all-removed result is the empty array {}.
export function arrayRemoveValue(arr: Value, elem: Value): Value {
  if (arr.kind !== "array") return nullValue();
  if (arr.dims.length > 1) {
    throw engineError(
      "feature_not_supported",
      "removing elements from multidimensional arrays is not supported",
    );
  }
  const kept = arr.elements.filter((e) => !notDistinct(e, elem));
  if (kept.length === 0) return emptyArray();
  const lb = arr.lbounds.length > 0 ? arr.lbounds[0]! : 1;
  return { kind: "array", dims: [kept.length], lbounds: [lb], elements: kept };
}

// arrayReplaceValue is array_replace(a, from, to) (array-functions.md §8): substitute every element
// NOT DISTINCT FROM `from` with `to`. Works on any dimensionality (the shape is preserved). NULL
// array → NULL.
export function arrayReplaceValue(arr: Value, from: Value, to: Value): Value {
  if (arr.kind !== "array") return nullValue();
  const elements = arr.elements.map((e) => (notDistinct(e, from) ? to : e));
  return {
    kind: "array",
    dims: [...arr.dims],
    lbounds: [...arr.lbounds],
    elements,
  };
}

// arrayPositionValue is array_position(a, e[, start]) (array-functions.md §8): the SUBSCRIPT (in the
// array's lower-bound space) of the first element NOT DISTINCT FROM e, NULL if absent. 1-D/empty only
// (a multidimensional array is 0A000); the optional start is a subscript to begin at, and a NULL
// start is 22004.
export function arrayPositionValue(arr: Value, elem: Value, start: Value | null): Value {
  if (arr.kind !== "array") return nullValue();
  if (arr.dims.length > 1) {
    throw engineError(
      "feature_not_supported",
      "searching for elements in multidimensional arrays is not supported",
    );
  }
  const lb = arr.lbounds.length > 0 ? arr.lbounds[0]! : 1;
  let begin = 0;
  if (start !== null) {
    if (start.kind === "null")
      throw engineError("null_value_not_allowed", "initial position must not be null");
    const off = Number((start as { int: bigint }).int) - lb;
    if (off > 0) begin = off;
  }
  for (let i = begin; i < arr.elements.length; i++) {
    if (notDistinct(arr.elements[i]!, elem)) return intValue(BigInt(lb + i));
  }
  return nullValue();
}

// arrayPositionsValue is array_positions(a, e) (array-functions.md §8): the i32[] of every match's
// subscript (in the array's lower-bound space), the empty array {} if none. NULL array → NULL;
// 1-D/empty only (a multidimensional array is 0A000).
export function arrayPositionsValue(arr: Value, elem: Value): Value {
  if (arr.kind !== "array") return nullValue();
  if (arr.dims.length > 1) {
    throw engineError(
      "feature_not_supported",
      "searching for elements in multidimensional arrays is not supported",
    );
  }
  const lb = arr.lbounds.length > 0 ? arr.lbounds[0]! : 1;
  const positions: Value[] = [];
  for (let i = 0; i < arr.elements.length; i++) {
    if (notDistinct(arr.elements[i]!, elem)) positions.push(intValue(BigInt(lb + i)));
  }
  return arrayValue(positions);
}

// arrayDimsText is the array_dims text form `[l1:u1][l2:u2]…` (no trailing `=`, unlike array_out's
// prefix — array-functions.md §3.1).
export function arrayDimsText(a: { dims: number[]; lbounds: number[] }): string {
  let s = "";
  for (let d = 0; d < a.dims.length; d++) s += "[" + a.lbounds[d] + ":" + arrayUbound(a, d) + "]";
  return s;
}

// arrayExtend is array_append (atEnd=true) / array_prepend (array-functions.md §3.2). The array side
// is non-strict: a NULL or empty array yields the 1-D singleton {elem} (lower bound 1). A 1-D array
// grows by one element, preserving its lower bound; a multidimensional array is 22000.
export function arrayExtend(arr: Value, elem: Value, atEnd: boolean): Value {
  if (arr.kind !== "array" || arr.dims.length === 0) return arrayValue([elem]);
  if (arr.dims.length !== 1) {
    throw engineError("data_exception", "argument must be empty or one-dimensional array");
  }
  const elements = atEnd ? [...arr.elements, elem] : [elem, ...arr.elements];
  return {
    kind: "array",
    dims: [arr.dims[0]! + 1],
    lbounds: [...arr.lbounds],
    elements,
  };
}

// arrayCatValues is array_cat (array-functions.md §3.2): identity-aware concatenation along the outer
// dimension. NULL/empty is the identity (both NULL → NULL). Same dimensionality concatenates if the
// inner dims match; an off-by-one dimensionality appends/prepends the lower one as an outer slice; any
// other pairing — or an inner-dim mismatch — is 2202E. The flattened element list is always a ++ b
// (row-major, outer-first); the result lower bounds come from the higher-dim operand.
export function arrayCatValues(a: Value, b: Value): Value {
  if (a.kind === "null" && b.kind === "null") return nullValue();
  if (a.kind === "null") return b;
  if (b.kind === "null") return a;
  if (a.kind !== "array" || b.kind !== "array") return nullValue(); // unreachable (resolver gate)
  if (a.dims.length === 0) return b;
  if (b.dims.length === 0) return a;
  const mismatch = () =>
    engineError("array_subscript_error", "cannot concatenate incompatible arrays");
  const eqInts = (x: number[], y: number[]): boolean =>
    x.length === y.length && x.every((v, i) => v === y[i]);
  const elements = [...a.elements, ...b.elements];
  const na = a.dims.length;
  const nb = b.dims.length;
  if (na === nb) {
    if (!eqInts(a.dims.slice(1), b.dims.slice(1))) throw mismatch();
    const dims = [...a.dims];
    dims[0] = a.dims[0]! + b.dims[0]!;
    return { kind: "array", dims, lbounds: [...a.lbounds], elements };
  }
  if (na === nb + 1) {
    if (!eqInts(a.dims.slice(1), b.dims)) throw mismatch();
    const dims = [...a.dims];
    dims[0] = a.dims[0]! + 1;
    return { kind: "array", dims, lbounds: [...a.lbounds], elements };
  }
  if (nb === na + 1) {
    if (!eqInts(b.dims.slice(1), a.dims)) throw mismatch();
    const dims = [...b.dims];
    dims[0] = b.dims[0]! + 1;
    return { kind: "array", dims, lbounds: [...b.lbounds], elements };
  }
  throw mismatch();
}

// buildNestedArray stacks the evaluated elements of a nested ARRAY[...] constructor into a value of
// one higher dimension (spec/design/array.md §4). The resolver guarantees every item is an array; a
// NULL sub-array or a sub-array of differing shape is a 2202E. Stacking empty sub-arrays yields the
// empty array (PG: ARRAY['{}'::int[]] → {}).
export function buildNestedArray(subs: Value[]): Value {
  const mismatch = "multidimensional arrays must have array expressions with matching dimensions";
  const arrs = subs.map((sv) => {
    if (sv.kind === "array") return sv;
    if (sv.kind === "null") throw arraySubscriptErr(mismatch);
    throw typeError("internal: nested array constructor over a non-array");
  });
  const eqNum = (a: number[], b: number[]): boolean =>
    a.length === b.length && a.every((x, i) => x === b[i]);
  const dims0 = arrs[0]!.dims;
  const lbounds0 = arrs[0]!.lbounds;
  for (const a of arrs.slice(1)) {
    if (!eqNum(a.dims, dims0) || !eqNum(a.lbounds, lbounds0)) throw arraySubscriptErr(mismatch);
  }
  if (dims0.length === 0) return emptyArray(); // all sub-arrays empty → empty array
  const elements: Value[] = [];
  for (const a of arrs) elements.push(...a.elements);
  return {
    kind: "array",
    dims: [arrs.length, ...dims0],
    lbounds: [1, ...lbounds0],
    elements,
  };
}

// evalSubscript evaluates an array subscript `base[..][..]` (spec/design/array.md §6). A NULL array
// or any NULL subscript bound yields NULL; element access returns the element (or NULL), slice
// access a (renumbered) sub-array.
export function evalSubscript(
  e: { base: RExpr; subscripts: RSubscript[]; isSlice: boolean },
  row: Row,
  env: EvalEnv,
  m: Meter,
): Value {
  const base = evalExpr(e.base, row, env, m);
  if (base.kind === "null") return nullValue();
  if (base.kind !== "array") throw typeError("internal: subscript on a non-array value");
  if (e.isSlice) {
    // Per-dimension (lower, upper); a scalar index i becomes 1:i (PG), an omitted bound defers to
    // the array's own bound (null lo/hi). A NULL bound → NULL.
    const los: (bigint | null)[] = [];
    const his: (bigint | null)[] = [];
    for (const s of e.subscripts) {
      if (!s.isSlice) {
        const v = evalExpr(s.index, row, env, m);
        if (v.kind === "null") return nullValue();
        if (v.kind !== "int") throw typeError("internal: non-integer array subscript");
        los.push(1n); // scalar i → 1:i
        his.push(v.int);
      } else {
        const lo = evalOptBound(s.lower, row, env, m);
        if (lo === "null") return nullValue();
        const hi = evalOptBound(s.upper, row, env, m);
        if (hi === "null") return nullValue();
        los.push(lo);
        his.push(hi);
      }
    }
    return arrayGetSlice(base, los, his);
  }
  // Element access: every spec is an index.
  const idxs: bigint[] = [];
  for (const s of e.subscripts) {
    if (s.isSlice) throw typeError("internal: slice spec in element access");
    const v = evalExpr(s.index, row, env, m);
    if (v.kind === "null") return nullValue();
    if (v.kind !== "int") throw typeError("internal: non-integer array subscript");
    idxs.push(v.int);
  }
  return arrayGetElement(base, idxs);
}

// evalOptBound evaluates an optional slice-bound expression: null expr → null (defer to the array
// bound); a NULL value → "null" (the whole result is NULL); an integer → its bigint.
export function evalOptBound(
  e: RExpr | null,
  row: Row,
  env: EvalEnv,
  m: Meter,
): bigint | null | "null" {
  if (e === null) return null;
  const v = evalExpr(e, row, env, m);
  if (v.kind === "null") return "null";
  if (v.kind !== "int") throw typeError("internal: non-integer array slice bound");
  return v.int;
}

// arrayGetElement reads a single array element by idxs (1-based per dimension, using the value's
// lower bounds) — spec/design/array.md §6. NULL when the subscript count ≠ ndim or any index is out
// of range.
export function arrayGetElement(
  a: { dims: number[]; lbounds: number[]; elements: Value[] },
  idxs: bigint[],
): Value {
  const ndim = arrayNdim(a);
  if (idxs.length !== ndim || a.elements.length === 0) return nullValue();
  let flat = 0;
  let stride = 1;
  for (let d = ndim - 1; d >= 0; d--) {
    const lb = BigInt(a.lbounds[d]!);
    const ub = BigInt(arrayUbound(a, d));
    if (idxs[d]! < lb || idxs[d]! > ub) return nullValue();
    flat += Number(idxs[d]! - lb) * stride;
    stride *= a.dims[d]!;
  }
  return a.elements[flat]!;
}

// arrayGetSlice reads an array slice (spec/design/array.md §6): per-dimension requested (lower,
// upper) bounds (null defers to the value's own bound), clamped to each dimension's [lb,ub]. Too many
// subscripts, an empty source, or any empty clamped dimension yields the empty array; fewer
// subscripts than ndim leave the trailing dimensions at full range. The result is renumbered to lower
// bound 1 on every dimension (PG array_get_slice).
export function arrayGetSlice(
  a: { dims: number[]; lbounds: number[]; elements: Value[] },
  los: (bigint | null)[],
  his: (bigint | null)[],
): Value {
  const ndim = arrayNdim(a);
  if (los.length > ndim || ndim === 0) return emptyArray();
  const newDims: number[] = new Array(ndim);
  const starts: number[] = new Array(ndim); // source 0-based start per dimension
  for (let d = 0; d < ndim; d++) {
    const lb = BigInt(a.lbounds[d]!);
    const ub = BigInt(arrayUbound(a, d));
    let reqLo = lb;
    let reqHi = ub;
    if (d < los.length) {
      if (los[d] !== null) reqLo = los[d]!;
      if (his[d] !== null) reqHi = his[d]!;
    }
    const lo = reqLo < lb ? lb : reqLo;
    const hi = reqHi > ub ? ub : reqHi;
    if (lo > hi) return emptyArray(); // any empty dimension → empty slice
    newDims[d] = Number(hi - lo + 1n);
    starts[d] = Number(lo - lb);
  }
  // Row-major strides over the SOURCE array.
  const strides: number[] = new Array(ndim);
  strides[ndim - 1] = 1;
  for (let d = ndim - 2; d >= 0; d--) strides[d] = strides[d + 1]! * a.dims[d + 1]!;
  let total = 1;
  for (const d of newDims) total *= d;
  const elements: Value[] = new Array(total);
  const counter: number[] = new Array(ndim).fill(0);
  for (let k = 0; k < total; k++) {
    let flat = 0;
    for (let d = 0; d < ndim; d++) flat += (starts[d]! + counter[d]!) * strides[d]!;
    elements[k] = a.elements[flat]!;
    for (let d = ndim - 1; d >= 0; d--) {
      counter[d]!++;
      if (counter[d]! < newDims[d]!) break;
      counter[d] = 0;
    }
  }
  return {
    kind: "array",
    dims: newDims,
    lbounds: new Array(ndim).fill(1),
    elements,
  };
}

// unifyCaseTypes unifies a CASE's result-arm types (the THEN results + the ELSE, or "null" for an
// implicit ELSE) into one common type (spec/design/grammar.md §23): NULL-typed arms are dropped
// (they adapt); an all-NULL CASE is text (PostgreSQL). The non-NULL arms must share a family — all
// numeric unify to decimal if any is decimal, else the widest integer (the promotion tower);
// otherwise they must all be the same non-numeric family (text/boolean/bytea). A cross-family mix
// is 42804.
export function unifyCaseTypes(arms: ResolvedType[]): ResolvedType {
  const nonNull = arms.filter((t) => t.kind !== "null");
  if (nonNull.length === 0) return { kind: "text" }; // every arm NULL/untyped → text
  let allNumeric = true;
  let anyDecimal = false;
  for (const t of nonNull) {
    if (t.kind !== "int" && t.kind !== "decimal") allNumeric = false;
    if (t.kind === "decimal") anyDecimal = true;
  }
  if (allNumeric) {
    if (anyDecimal) return { kind: "decimal" };
    // All integer: the widest via the promotion tower (width is unobservable in output — every
    // integer renders under the `I` tag — but the fold keeps the type precise).
    let acc = nonNull[0]!;
    for (const t of nonNull.slice(1)) acc = { kind: "int", ty: promote(acc, t) };
    return acc;
  }
  // All float: the widest via the float tower (f32 + f64 → f64). A float mixed with a
  // non-float arm is a cross-family 42804 (caught by the same-family check below — float is a strict
  // island, no int/decimal reconciliation, float.md §6).
  if (nonNull.every((t) => t.kind === "float")) {
    let acc = (nonNull[0] as { kind: "float"; ty: ScalarType }).ty;
    for (const t of nonNull.slice(1)) acc = promoteFloat(acc, (t as { ty: ScalarType }).ty);
    return { kind: "float", ty: acc };
  }
  // Non-numeric: every arm must be the same family as the first (cross-family is 42804).
  const first = nonNull[0]!;
  for (const t of nonNull.slice(1)) {
    if (t.kind !== first.kind) throw typeError("CASE result types must be compatible");
  }
  return first;
}

// coerceCaseValue coerces a CASE arm's value to the unified result type. The only runtime
// coercion needed is widening an integer result to decimal when the unified type is decimal —
// integer-width unification needs none (all integers are bigint), and an all-NULL CASE is text but
// every arm evaluates to NULL anyway.
export function coerceCaseValue(v: Value, toDecimal: boolean): Value {
  if (toDecimal && v.kind === "int") return decimalValue(Decimal.fromBigInt(v.int));
  return v;
}

// setopName is the operator's name for an error message (PostgreSQL phrasing).
export function setopName(op: SetOpKind): string {
  return op === "union" ? "UNION" : op === "intersect" ? "INTERSECT" : "EXCEPT";
}

// unifySetopColumn unifies one output column's type across the two operands of a set operation
// (spec/design/grammar.md §25, types.md §4): integer widths promote to the widest; integer with
// decimal -> decimal; a NULL-typed operand takes the other's type (an all-NULL column stays "null"
// — PostgreSQL would call a top-level one text, but the type is never observed in output); a
// same-family non-numeric pair gives that type; anything else is 42804. The set of unifiable pairs
// mirrors the comparability matrix (compare.toml).
// unifyValuesColumn unifies two row value types for the SAME VALUES-body column
// (spec/design/grammar.md §42), the set-operation rule (§25): integer widths widen, int+decimal ->
// decimal, anything + NULL keeps the other, and a same-type scalar pair (text, boolean, bytea, uuid,
// a timestamp / timestamptz, an interval, a same-width float) unifies to itself; any other pair —
// including a composite or array column across rows (a deferred edge) — is 42804. Enumerated
// EXPLICITLY (not a generic same-kind passthrough) so all three cores compute byte-identical
// results (CLAUDE.md §8).
export function unifyValuesColumn(a: ResolvedType, b: ResolvedType): ResolvedType {
  if (a.kind === "null" && b.kind === "null") return { kind: "null" };
  if (a.kind === "null") return b;
  if (b.kind === "null") return a;
  if (a.kind === "int" && b.kind === "int") return { kind: "int", ty: promote(a, b) };
  if ((a.kind === "int" || a.kind === "decimal") && (b.kind === "int" || b.kind === "decimal")) {
    return { kind: "decimal" };
  }
  if (a.kind === "float" && b.kind === "float" && a.ty === b.ty) return a;
  if (
    a.kind === b.kind &&
    (a.kind === "text" ||
      a.kind === "bool" ||
      a.kind === "bytea" ||
      a.kind === "uuid" ||
      a.kind === "timestamp" ||
      a.kind === "timestamptz" ||
      a.kind === "interval" ||
      a.kind === "date")
  ) {
    return a;
  }
  throw engineError(
    "datatype_mismatch",
    `VALUES types ${rtName(a)} and ${rtName(b)} cannot be matched`,
  );
}

// scalarForParamHint is the scalar type to note a bind parameter at, given its VALUES column's
// unified type (spec/design/grammar.md §42). A scalar type flows through; a NULL / composite / array
// column has no scalar parameter type, so null is returned and the parameter stays untyped (42P18 at
// finalize).
export function scalarForParamHint(rt: ResolvedType): ScalarType | null {
  switch (rt.kind) {
    case "int":
    case "float":
      return rt.ty;
    case "bool":
      return "boolean";
    case "text":
      return "text";
    case "decimal":
      return "decimal";
    case "bytea":
      return "bytea";
    case "uuid":
      return "uuid";
    case "timestamp":
      return "timestamp";
    case "timestamptz":
      return "timestamptz";
    case "date":
      return "date";
    case "interval":
      return "interval";
    case "json":
      return "json";
    case "jsonb":
      return "jsonb";
    case "jsonpath":
      return "jsonpath";
    default:
      return null;
  }
}

export function unifySetopColumn(a: ResolvedType, b: ResolvedType, op: SetOpKind): ResolvedType {
  if (a.kind === "null" && b.kind === "null") return { kind: "null" };
  if (a.kind === "null") return b;
  if (b.kind === "null") return a;
  if (a.kind === "int" && b.kind === "int") return { kind: "int", ty: promote(a, b) };
  if ((a.kind === "int" || a.kind === "decimal") && (b.kind === "int" || b.kind === "decimal")) {
    // at least one decimal (both-int handled above) -> decimal
    return { kind: "decimal" };
  }
  // Two floats unify to the widest (the float tower — f32 + f64 → f64; the narrower
  // operand's rows are widened in coerceSetopRows). float never reconciles with int/decimal.
  if (a.kind === "float" && b.kind === "float")
    return { kind: "float", ty: promoteFloat(a.ty, b.ty) };
  if (a.kind === b.kind) return a;
  throw engineError(
    "datatype_mismatch",
    `${setopName(op)} types ${rtName(a)} and ${rtName(b)} cannot be matched`,
  );
}

// coerceSetopRows converts each row's values in place to the unified set-operation column types —
// the only runtime change is integer -> decimal (a NULL stays NULL; integer-width promotion is a
// value no-op since every integer is bigint). Same conversion coerceCaseValue uses for CASE.
export function coerceSetopRows(rows: Value[][], from: ResolvedType[], to: ResolvedType[]): void {
  for (let i = 0; i < to.length; i++) {
    if (from[i]!.kind === "int" && to[i]!.kind === "decimal") {
      for (const row of rows) {
        const v = row[i]!;
        if (v.kind === "int") row[i] = decimalValue(Decimal.fromBigInt(v.int));
      }
    }
    // f32 → f64 widening (lossless): the column unified to f64 but this operand is
    // f32, so its values become f64 Values (the number is already an exact binary64).
    const t = to[i]!;
    if (from[i]!.kind === "float" && t.kind === "float" && t.ty === "f64") {
      for (const row of rows) {
        const v = row[i]!;
        if (v.kind === "f32") row[i] = float64Value(v.value);
      }
    }
  }
}

// combineSetop combines the operands' rows per the set operator + ALL flag (spec/design/grammar.md
// §25). Rows match by the NULL-safe, value-canonical distinctRowKey (two NULLs match, 1.5 == 1.50,
// and a converted int matches the decimal). The emitted representative for a matched / deduplicated
// key is its FIRST occurrence scanning the LEFT operand then the right, and emitted rows keep that
// left-then-right scan order — deterministic and identical across cores. (A later ORDER BY
// re-sorts; without one, output order is unspecified and the corpus compares rowsort.)
export function combineSetop(
  op: SetOpKind,
  all: boolean,
  left: Value[][],
  right: Value[][],
): Value[][] {
  if (op === "union" && all) return left.concat(right);
  if (op === "union") {
    const seen = new Set<string>();
    const out: Value[][] = [];
    for (const row of left.concat(right)) {
      const k = distinctRowKey(row);
      if (!seen.has(k)) {
        seen.add(k);
        out.push(row);
      }
    }
    return out;
  }
  if (op === "intersect" && all) {
    const counts = new Map<string, number>();
    for (const row of right) {
      const k = distinctRowKey(row);
      counts.set(k, (counts.get(k) ?? 0) + 1);
    }
    const out: Value[][] = [];
    for (const row of left) {
      const k = distinctRowKey(row);
      const c = counts.get(k) ?? 0;
      if (c > 0) {
        counts.set(k, c - 1);
        out.push(row);
      }
    }
    return out;
  }
  if (op === "intersect") {
    const rightSet = new Set<string>();
    for (const row of right) rightSet.add(distinctRowKey(row));
    const emitted = new Set<string>();
    const out: Value[][] = [];
    for (const row of left) {
      const k = distinctRowKey(row);
      if (rightSet.has(k) && !emitted.has(k)) {
        emitted.add(k);
        out.push(row);
      }
    }
    return out;
  }
  if (op === "except" && all) {
    const counts = new Map<string, number>();
    for (const row of right) {
      const k = distinctRowKey(row);
      counts.set(k, (counts.get(k) ?? 0) + 1);
    }
    const out: Value[][] = [];
    for (const row of left) {
      const k = distinctRowKey(row);
      const c = counts.get(k) ?? 0;
      if (c > 0) counts.set(k, c - 1);
      else out.push(row);
    }
    return out;
  }
  // EXCEPT, distinct
  const rightSet = new Set<string>();
  for (const row of right) rightSet.add(distinctRowKey(row));
  const emitted = new Set<string>();
  const out: Value[][] = [];
  for (const row of left) {
    const k = distinctRowKey(row);
    if (!rightSet.has(k) && !emitted.has(k)) {
      emitted.add(k);
      out.push(row);
    }
  }
  return out;
}

// resolveSetopOrderKey resolves a trailing ORDER BY key for a set operation against the OUTPUT
// column names (the left operand's). A qualified key is 42P01 (no relation scope after a set
// operation); an unknown name is 42703. Returns the output column index.
export function resolveSetopOrderKey(key: OrderKey, names: string[]): number {
  // A set-operation ORDER BY accepts only an output column name or ordinal — a general expression key
  // (after the inputs are unified) is 0A000, matching PostgreSQL's "invalid UNION/INTERSECT/EXCEPT
  // ORDER BY clause" (grammar.md §10).
  if (key.expr !== null) {
    throw engineError("feature_not_supported", "invalid UNION/INTERSECT/EXCEPT ORDER BY clause");
  }
  // An output-column ordinal (`... ORDER BY 1`) resolves by position into the output columns; out of
  // [1, ncols] is 42P10 (grammar.md §10). It precedes the name path (an ordinal has no column).
  if (key.ordinal !== null) {
    const ord = key.ordinal;
    if (ord < 1 || ord > names.length) {
      throw engineError(
        "invalid_column_reference",
        `ORDER BY position ${ord} is not in select list`,
      );
    }
    return ord - 1;
  }
  if (key.qualifier !== null) {
    throw engineError("undefined_table", "missing FROM-clause entry for table " + key.qualifier);
  }
  const idx = names.findIndex((n) => n.toLowerCase() === key.column.toLowerCase());
  if (idx < 0) throw engineError("undefined_column", "column " + key.column + " does not exist");
  return idx;
}

// exprEqual reports whether two parsed expression trees are STRUCTURALLY equal (spec/design/grammar.md
// §10) — the TS equivalent of the Rust core's derived PartialEq on Expr (and the Go core's
// reflect.DeepEqual). Used by the SELECT DISTINCT ORDER BY restriction to decide whether an expression
// sort key matches a select-list expression. The AST carries no source positions, so textually-
// identical fragments (`a + b` here and there) compare equal; the recursion descends arrays and the
// discriminated-union nodes, comparing primitives (incl. bigint and a Decimal's fields) by value.
export function exprEqual(a: Expr, b: Expr): boolean {
  return astDeepEqual(a, b);
}

export function astDeepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true; // identical primitives (incl. equal bigints) and the same reference
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  const aArr = Array.isArray(a);
  if (aArr !== Array.isArray(b)) return false;
  if (aArr) {
    const aa = a as unknown[];
    const bb = b as unknown[];
    if (aa.length !== bb.length) return false;
    for (let i = 0; i < aa.length; i++) if (!astDeepEqual(aa[i], bb[i])) return false;
    return true;
  }
  const ao = a as Record<string, unknown>;
  const bo = b as Record<string, unknown>;
  const ak = Object.keys(ao);
  if (ak.length !== Object.keys(bo).length) return false;
  for (const k of ak) {
    if (!Object.prototype.hasOwnProperty.call(bo, k)) return false;
    if (!astDeepEqual(ao[k], bo[k])) return false;
  }
  return true;
}

// requireAssignable: a value assigned to a column must match its family — an integer column
// takes an integer (or NULL); a decimal column takes an integer (int→decimal implicit) or
// decimal (or NULL); a text column takes a text (or NULL); a boolean column takes a boolean
// (or NULL). A decimal value into an integer column is NOT assignable (decimal→int is
// explicit-CAST only). Any cross-family pair is a 42804 type error. Mirrors the INSERT literal
// type-check, generalized to expressions.
export function requireAssignable(t: ResolvedType, colTy: ScalarType, col: string): void {
  let ok: boolean;
  if (isInteger(colTy)) ok = t.kind === "int" || t.kind === "null";
  else if (isDecimal(colTy)) ok = t.kind === "int" || t.kind === "decimal" || t.kind === "null";
  // A float column accepts a float value of EQUAL OR NARROWER width (f32 → f64 widening is
  // implicit; f64 → f32 needs an explicit CAST — float.md §6) or NULL. No int/decimal.
  else if (isFloat(colTy))
    ok = (t.kind === "float" && promoteFloat(t.ty, colTy) === colTy) || t.kind === "null";
  else if (isBool(colTy)) ok = t.kind === "bool" || t.kind === "null";
  else if (isBytea(colTy)) ok = t.kind === "bytea" || t.kind === "null";
  else if (isUuid(colTy)) ok = t.kind === "uuid" || t.kind === "null";
  else if (isTimestamp(colTy)) ok = t.kind === "timestamp" || t.kind === "null";
  else if (isTimestamptz(colTy)) ok = t.kind === "timestamptz" || t.kind === "null";
  else if (isInterval(colTy)) ok = t.kind === "interval" || t.kind === "null";
  else if (isDate(colTy)) ok = t.kind === "date" || t.kind === "null";
  else ok = t.kind === "text" || t.kind === "null";
  if (!ok) {
    throw typeError("cannot assign a value to column " + col + " of type " + canonicalName(colTy));
  }
}

// resolveTypeAndTypmod resolves a column-definition or CAST target type name + optional type
// modifier. All canonical names and aliases (including boolean/bool and numeric/decimal/dec)
// resolve here; a genuinely unknown name is a 42704. A type modifier is meaningful only for
// decimal (validated to numeric(p,s) — 22023); on any other type it is 0A000 (varchar(n) and
// other parameterized types are deferred — spec/design/grammar.md §14). Type-specific narrowings
// (a text/boolean/decimal PRIMARY KEY, a CAST to text/boolean) are enforced at the call site.
// MAX_VARCHAR_LEN is PostgreSQL's varchar(n) ceiling (spec/design/types.md §15); stored on disk
// as a u32.
export const MAX_VARCHAR_LEN = 10485760;

// resolveTypeAndTypmod resolves a scalar type name + optional type modifier, returning the type,
// the decimal typmod (decimal), and the varchar(n) max length (text — spec/design/types.md §15).
// At most one typmod is ever non-null (they belong to different types); a typmod on any other type
// is 0A000.
export function resolveTypeAndTypmod(
  name: string,
  typeMod: TypeMod | null,
): [ScalarType, DecimalTypmod | null, number | null] {
  const ty = scalarTypeFromName(name);
  if (ty === undefined) {
    throw engineError("undefined_object", "type does not exist: " + name);
  }
  if (typeMod === null) return [ty, null, null];
  if (isDecimal(ty)) return [ty, validateDecimalTypmod(typeMod), null];
  if (isText(ty)) return [ty, null, validateVarcharTypmod(typeMod)];
  throw engineError(
    "feature_not_supported",
    "a type modifier is not supported for type " + canonicalName(ty),
  );
}

// validateVarcharTypmod validates a varchar(n) type modifier: 1 <= n <= 10485760 (PostgreSQL's
// varchar ceiling), else trap 22023 (spec/design/types.md §15). A scale (varchar(n,m)) is a syntax
// error — varchar takes a single length argument.
export function validateVarcharTypmod(tm: TypeMod): number {
  if (tm.scale !== null) {
    throw engineError("syntax_error", "varchar takes exactly one type modifier (a length)");
  }
  const n = tm.precision;
  if (n < 1n) {
    throw engineError("invalid_parameter_value", "length for type varchar must be at least 1");
  }
  if (n > BigInt(MAX_VARCHAR_LEN)) {
    throw engineError(
      "invalid_parameter_value",
      `length for type varchar cannot exceed ${MAX_VARCHAR_LEN}`,
    );
  }
  return Number(n);
}

// validateDecimalTypmod validates a decimal numeric(p[,s]) type modifier: 1 <= p <= 1000,
// 0 <= s <= p; else trap 22023 (spec/design/decimal.md §2). numeric(p) means scale 0.
export function validateDecimalTypmod(tm: TypeMod): DecimalTypmod {
  const p = tm.precision;
  if (p < 1n || p > BigInt(MAX_PRECISION)) {
    throw engineError(
      "invalid_parameter_value",
      `NUMERIC precision ${p} must be between 1 and ${MAX_PRECISION}`,
    );
  }
  const s = tm.scale ?? 0n;
  if (s > p || s > BigInt(MAX_SCALE)) {
    throw engineError(
      "invalid_parameter_value",
      `NUMERIC scale ${s} must be between 0 and precision ${p}`,
    );
  }
  return { precision: Number(p), scale: Number(s) };
}
