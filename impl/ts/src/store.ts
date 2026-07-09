// storeValue coerces a value into a column for storage (shared by INSERT and UPDATE). NULL
// honours NOT NULL (23502); an integer into an integer column is range-checked (22003); an
// integer into a decimal column widens (int→decimal) then coerces to the typmod; a decimal into
// a decimal column coerces to the typmod (rounds, precision-checks → 22003); a boolean into a
// boolean column is accepted as-is; a cross-family value (decimal→int, text→int, etc.) is 42804.
import type { ArrayInResult, Value } from "./value.ts";
import type { DecimalTypmod, ScalarType } from "./types.ts";
import { engineError, notNullViolation } from "./errors.ts";
import {
  arrayValue,
  boolNot,
  boolValue,
  byteaValue,
  compositeValue,
  dateValue,
  decimalValue,
  eq3,
  float32Value,
  float64Value,
  intValue,
  intervalValue,
  jsonPathValue,
  jsonValue,
  jsonbValue,
  nullValue,
  parseArrayLiteral,
  parseRecordTokens,
  rangeValue,
  textValue,
  timestampValue,
  timestamptzValue,
  uuidValue,
} from "./value.ts";
import {
  canonicalName,
  inRange,
  isBool,
  isBytea,
  isDate,
  isDecimal,
  isFloat,
  isInteger,
  isInterval,
  isJson,
  isJsonb,
  isText,
  isTimestamp,
  isTimestamptz,
  isUuid,
} from "./types.ts";
import { Decimal } from "./decimal.ts";
import { decimalCmpWork, makeFloat } from "./eval.ts";
import {
  arraySubscriptErr,
  buildNestedArray,
  coerceStringLiteral,
  coerceStringToRange,
  decodeByteaLiteral,
  decodeUuidLiteral,
} from "./executor.ts";
import { NEG_INFINITY, POS_INFINITY, parseTimestamp, parseTimestamptz } from "./timestamp.ts";
import { DATE_NEG_INFINITY, DATE_POS_INFINITY, parseDate } from "./date.ts";
import { parseInterval, tsShift } from "./interval.ts";
import { jsonbIn, validateJson } from "./json.ts";
import type { BinaryOp, InsertValue, Literal } from "./ast.ts";
import type { ColField, ColType } from "./catalog.ts";
import { rangeForElement } from "./range.ts";
import type { EvalEnv, RExpr } from "./executor.ts";
import type { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import type { Interval } from "./interval.ts";
import type { ZoneRef } from "./timezone.ts";
import { instantToLocalMicros, localToInstantMicros } from "./timezone.ts";
export function storeValue(
  v: Value,
  colTy: ScalarType,
  typmod: DecimalTypmod | null,
  varcharLen: number | null,
  notNull: boolean,
  colName: string,
): Value {
  switch (v.kind) {
    case "null":
      if (notNull) {
        throw notNullViolation(colName);
      }
      return nullValue();
    case "int":
      if (isInteger(colTy)) {
        if (!inRange(colTy, v.int)) throw overflow(colTy);
        return intValue(v.int);
      }
      if (isDecimal(colTy)) return decimalValue(coerceDecimal(Decimal.fromBigInt(v.int), typmod));
      // An integer LITERAL adapts to a float column (float.md §4 literal adaptation — INSERT VALUES /
      // DEFAULT bypass the expression resolver, so the adaptation lands here, like text→bytea). This
      // is literal adaptation, NOT an implicit cross-family cast of a value (storing a f64 into a
      // f32 is still rejected below). Out of binary range → 22003 (the finite-overflow rule).
      if (isFloat(colTy)) return makeFloat(colTy, Number(v.int));
      throw typeError(
        "cannot store an integer value in " + canonicalName(colTy) + " column " + colName,
      );
    case "decimal":
      if (isDecimal(colTy)) return decimalValue(coerceDecimal(v.dec, typmod));
      // A decimal LITERAL adapts to a float column (float.md §4): nearest binary, fround for f32.
      if (isFloat(colTy)) {
        const d = Number(v.dec.render());
        if (!Number.isFinite(d)) throw overflow(colTy);
        return makeFloat(colTy, d);
      }
      throw typeError(
        "cannot store a decimal value in " + canonicalName(colTy) + " column " + colName,
      );
    case "f32":
      // f32 into f32 stores as-is; into f64 widens losslessly (every binary32 is an
      // exact binary64 — float.md §2). Bits (incl -0/NaN) preserved. No cross-family store.
      if (colTy === "f32") return v;
      if (colTy === "f64") return float64Value(v.value);
      throw typeError("cannot store a f32 value in " + canonicalName(colTy) + " column " + colName);
    case "f64":
      // f64 into f64 stores as-is. f64 → f32 is LOSSY (explicit cast required, not a
      // silent store) so it is rejected here (the resolver's assignableTo already gates it 42804).
      if (colTy === "f64") return v;
      throw typeError("cannot store a f64 value in " + canonicalName(colTy) + " column " + colName);
    case "text":
      if (isText(colTy)) {
        // A varchar(n) column enforces its length on store (assignment semantics): over-length
        // traps 22001, unless the excess is all spaces (truncate) — spec/design/types.md §15.
        return textValue(coerceVarcharStore(v.text, varcharLen, colName));
      }
      // A string literal adapts to a bytea column, decoding the hex input (types.md §6/§13);
      // malformed hex traps 22P02.
      if (isBytea(colTy)) return byteaValue(decodeByteaLiteral(v.text));
      // ... and to a uuid column via the PG-flexible uuid input (types.md §6/§14); 22P02 on bad input.
      if (isUuid(colTy)) return uuidValue(decodeUuidLiteral(v.text));
      // ... or to a timestamp column (spec/design/timestamp.md); bad input traps 22007/22008.
      if (isTimestamp(colTy)) return timestampValue(parseTimestamp(v.text));
      if (isTimestamptz(colTy)) return timestamptzValue(parseTimestamptz(v.text));
      if (isDate(colTy)) return dateValue(parseDate(v.text));
      // ... or to an interval column (spec/design/interval.md); bad input traps 22007/22008.
      if (isInterval(colTy)) return intervalValue(parseInterval(v.text));
      // ... or to a json column (spec/design/json.md §4): validate, store verbatim; malformed → 22P02.
      if (isJson(colTy)) {
        validateJson(v.text);
        return jsonValue(v.text);
      }
      // ... or to a jsonb column (§2): parse + canonicalize; malformed → 22P02.
      if (isJsonb(colTy)) return jsonbValue(jsonbIn(v.text));
      throw typeError(
        "cannot store a text value in " + canonicalName(colTy) + " column " + colName,
      );
    case "bytea":
      if (isBytea(colTy)) return v;
      throw typeError(
        "cannot store a bytea value in " + canonicalName(colTy) + " column " + colName,
      );
    case "uuid":
      if (isUuid(colTy)) return v;
      throw typeError(
        "cannot store a uuid value in " + canonicalName(colTy) + " column " + colName,
      );
    case "timestamp":
      if (isTimestamp(colTy)) return v;
      throw typeError(
        "cannot store a timestamp value in " + canonicalName(colTy) + " column " + colName,
      );
    case "timestamptz":
      if (isTimestamptz(colTy)) return v;
      throw typeError(
        "cannot store a timestamptz value in " + canonicalName(colTy) + " column " + colName,
      );
    case "date":
      if (isDate(colTy)) return v;
      throw typeError(
        "cannot store a date value in " + canonicalName(colTy) + " column " + colName,
      );
    case "interval":
      if (isInterval(colTy)) return v;
      throw typeError(
        "cannot store an interval value in " + canonicalName(colTy) + " column " + colName,
      );
    // A json/jsonb value stores into a json/jsonb column verbatim (J1); any other target is a 42804
    // type mismatch. In J0 no json/jsonb column exists, so this always errors.
    case "json":
      if (isJson(colTy)) return v;
      throw typeError(
        "cannot store a json value in " + canonicalName(colTy) + " column " + colName,
      );
    case "jsonb":
      if (isJsonb(colTy)) return v;
      throw typeError(
        "cannot store a jsonb value in " + canonicalName(colTy) + " column " + colName,
      );
    case "jsonpath":
      // A jsonpath value never stores into a column (a jsonpath column is 0A000 — literal-only).
      throw typeError(
        "cannot store a jsonpath value in " + canonicalName(colTy) + " column " + colName,
      );
    default: // bool
      if (isBool(colTy)) return v;
      throw typeError(
        "cannot store a boolean value in " + canonicalName(colTy) + " column " + colName,
      );
  }
}

// coerceDecimal coerces a decimal into a column's typmod: round to the declared scale and
// precision-check (22003) for numeric(p,s); for an unconstrained numeric column just cap-check.
export function coerceDecimal(d: Decimal, typmod: DecimalTypmod | null): Decimal {
  return typmod !== null ? d.coerceToTypmod(typmod.precision, typmod.scale) : d.checkCap();
}

// truncateToChars truncates a text value to at most n code points (the explicit varchar(n) cast
// rule — spec/design/types.md §15). JS strings are UTF-16, so the [...s] code-point iterator is
// used, NOT s.slice, which would split astral characters mid-surrogate.
export function truncateToChars(s: string, n: number): string {
  const cps = [...s];
  return cps.length <= n ? s : cps.slice(0, n).join("");
}

// coerceVarcharStore coerces a text value into a varchar(n) column/field for STORAGE (the
// assignment rule — spec/design/types.md §15): a value longer than n code points traps 22001,
// UNLESS every excess code point is a space (U+0020), in which case it is silently truncated to n
// (the SQL-standard trailing-space exception PostgreSQL implements). A null varcharLen (an
// unbounded text column) passes the value through unchanged.
export function coerceVarcharStore(s: string, varcharLen: number | null, colName: string): string {
  if (varcharLen === null) return s;
  const cps = [...s];
  if (cps.length <= varcharLen) return s;
  for (let i = varcharLen; i < cps.length; i++) {
    if (cps[i] !== " ") {
      throw engineError(
        "string_data_right_truncation",
        `value too long for type varchar(${varcharLen}) in column ${colName}`,
      )
        .withDataType(`varchar(${varcharLen})`)
        .withColumn(colName);
    }
  }
  return cps.slice(0, varcharLen).join("");
}

// literalToValue wraps a parsed literal as a runtime value (type-check/coercion is storeValue).
export function literalToValue(lit: Literal): Value {
  switch (lit.kind) {
    case "null":
      return nullValue();
    case "int":
      return intValue(lit.int);
    case "bool":
      return { kind: "bool", value: lit.value };
    case "text":
      return textValue(lit.text);
    default: // decimal
      return decimalValue(lit.dec);
  }
}

// coerceForStore coerces a value into a column of resolved ColType (spec/design/composite.md §4):
// a scalar dispatches to storeValue; a composite to storeComposite. The single store-coercion seam
// the INSERT/UPDATE paths use.
export function coerceForStore(
  v: Value,
  ty: ColType,
  typmod: DecimalTypmod | null,
  varcharLen: number | null,
  notNull: boolean,
  colName: string,
): Value {
  if (ty.kind === "scalar") return storeValue(v, ty.scalar, typmod, varcharLen, notNull, colName);
  if (ty.kind === "array") return storeArray(v, ty.elem, notNull, colName);
  if (ty.kind === "range") return storeRange(v, ty.elem, notNull, colName);
  return storeComposite(v, ty.name, ty.fields, notNull, colName);
}

// storeRange coerces a value into a RANGE column (spec/design/ranges.md §4): NULL honours NOT NULL
// (23502); a range value is already canonical + element-typed by the resolver (the literal/cast
// path canonicalized it), so each present finite bound is re-coerced to the element type as a
// belt-and-suspenders identity (an unconstrained scalar coercion — no typmod, NULL-tolerant) and
// the value passes through; any other value is a 42804. An infinite bound is null and skipped;
// bounds are never NULL here (a null bound is infinite, not NULL), so the element store is never
// NOT NULL.
export function storeRange(v: Value, elem: ColType, notNull: boolean, colName: string): Value {
  if (v.kind === "null") {
    if (notNull) {
      throw notNullViolation(colName);
    }
    return nullValue();
  }
  if (v.kind !== "range") {
    throw typeError("cannot store a non-range value in range column " + colName);
  }
  if (v.empty) return v;
  const coerce = (b: Value | null): Value | null =>
    b === null ? null : coerceForStore(b, elem, null, null, false, colName);
  return rangeValue(coerce(v.lower), coerce(v.upper), v.lowerInc, v.upperInc);
}

// storeArray coerces a value into an ARRAY column (spec/design/array.md §4): NULL honours NOT NULL
// (23502); an array value coerces each element to the declared element type via coerceForStore (a
// NULL element is allowed — array elements are nullable). Any other value is a 42804.
export function storeArray(v: Value, elem: ColType, notNull: boolean, colName: string): Value {
  if (v.kind === "null") {
    if (notNull) {
      throw notNullViolation(colName);
    }
    return nullValue();
  }
  if (v.kind !== "array") {
    throw typeError("cannot store a non-array value in array column " + colName);
  }
  // Elements are nullable; the element typmod is unconstrained this slice (numeric(p,s)[] deferred).
  // The shape (dims/lbounds) is preserved.
  const out = v.elements.map((el) => coerceForStore(el, elem, null, null, false, colName));
  return { kind: "array", dims: v.dims, lbounds: v.lbounds, elements: out };
}

// storeComposite coerces a value into a COMPOSITE column (spec/design/composite.md §4): NULL honours
// NOT NULL (23502); a composite value must have exactly the declared field count (42804) and each
// field is coerced to its declared field type via coerceForStore (recursing); any other value is a
// 42804. A NULL field of a NOT NULL composite field traps 23502.
export function storeComposite(
  v: Value,
  typeName: string,
  fields: ColField[],
  notNull: boolean,
  colName: string,
): Value {
  if (v.kind === "null") {
    if (notNull) {
      throw notNullViolation(colName);
    }
    return nullValue();
  }
  if (v.kind !== "composite") {
    throw typeError(
      "cannot store a non-record value in composite column " + colName + " (type " + typeName + ")",
    );
  }
  if (v.fields.length !== fields.length) {
    throw typeError(
      "row has " +
        v.fields.length +
        " fields but composite type " +
        typeName +
        " has " +
        fields.length,
    );
  }
  const out: Value[] = new Array(fields.length);
  for (let i = 0; i < fields.length; i++) {
    const f = fields[i]!;
    out[i] = coerceForStore(v.fields[i]!, f.type, f.typmod, f.varcharLen, f.notNull, f.name);
  }
  return compositeValue(out);
}

// materializeInsertValue materializes one INSERT VALUES slot into a Value against the column's
// resolved ColType (spec/design/composite.md §1/§4): a scalar slot is a literal or a bound $N; a
// composite slot is a ROW(…) whose fields recurse against the composite's field types, or a bound
// $N. The result is then fully coerced/range-checked by coerceForStore. DEFAULT is handled by the
// caller at the top level (it is not a valid field inside a ROW(…)).
export function materializeInsertValue(iv: InsertValue, ty: ColType, bound: Value[]): Value {
  if (ty.kind === "array") {
    switch (iv.kind) {
      case "array": {
        // ARRAY[e, …]: a nested constructor (an element is itself ARRAY[…]) stacks the sub-arrays
        // into a higher dimension (mirrors the evaluator's buildNestedArray, spec/design/array.md
        // §4); otherwise each element materializes against the element type into a flat 1-D array. A
        // scalar mixed with an array sub-element errors 42804 (materialized against the array type).
        if (iv.elements.some((el) => el.kind === "array")) {
          const subs = iv.elements.map((el) => materializeInsertValue(el, ty, bound));
          return buildNestedArray(subs);
        }
        const vals = iv.elements.map((el) => materializeInsertValue(el, ty.elem, bound));
        return arrayValue(vals);
      }
      case "param":
        return bound[iv.index - 1]!;
      case "row":
        throw typeError("cannot assign a record value to an array column");
      case "lit":
        // A bare string literal adapts to the array context via array_in (the same
        // string-adapts-to-context rule bytea/uuid use — spec/design/array.md §7).
        if (iv.lit.kind === "text") return coerceStringToArray(iv.lit.text, ty.elem);
        if (iv.lit.kind === "null") return nullValue();
        throw typeError("cannot assign a scalar value to an array column");
      default: // default
        throw engineError("syntax_error", "DEFAULT is not allowed inside ARRAY[...]");
    }
  }
  if (ty.kind === "range") {
    // A range column's element is always a scalar; the descriptor (for canonicalization) is
    // re-derived from it (spec/design/ranges.md §3/§4).
    if (ty.elem.kind !== "scalar")
      throw new Error("a range element is always a scalar (ranges.md §2)");
    const desc = rangeForElement(ty.elem.scalar);
    if (desc === undefined) throw new Error("a range column's element always has a range type");
    switch (iv.kind) {
      case "lit":
        // A bare string literal adapts to the range context via range_in (the same
        // string-adapts-to-context rule array/bytea/uuid use — spec/design/ranges.md §5).
        if (iv.lit.kind === "text") return coerceStringToRange(iv.lit.text, desc);
        if (iv.lit.kind === "null") return nullValue();
        throw typeError("cannot assign a scalar value to a range column");
      case "param":
        return bound[iv.index - 1]!;
      case "array":
        throw typeError("cannot assign an array value to a range column");
      case "row":
        throw typeError("cannot assign a record value to a range column");
      default: // default
        throw engineError("syntax_error", "DEFAULT is not allowed inside ROW(...)");
    }
  }
  if (ty.kind === "scalar") {
    switch (iv.kind) {
      case "lit":
        return literalToValue(iv.lit);
      case "param":
        return bound[iv.index - 1]!;
      case "row":
        throw typeError("cannot assign a record value to a " + canonicalName(ty.scalar) + " field");
      case "array":
        throw typeError("cannot assign an array value to a " + canonicalName(ty.scalar) + " field");
      default: // default
        throw engineError("syntax_error", "DEFAULT is not allowed inside ROW(...)");
    }
  }
  // ty is a composite column type.
  switch (iv.kind) {
    case "row": {
      if (iv.fields.length !== ty.fields.length) {
        throw typeError(
          "ROW has " +
            iv.fields.length +
            " fields but composite type " +
            ty.name +
            " has " +
            ty.fields.length,
        );
      }
      const vals: Value[] = new Array(ty.fields.length);
      for (let i = 0; i < ty.fields.length; i++)
        vals[i] = materializeInsertValue(iv.fields[i]!, ty.fields[i]!.type, bound);
      return compositeValue(vals);
    }
    case "param":
      return bound[iv.index - 1]!;
    case "lit":
      throw typeError("cannot assign a scalar value to composite column (type " + ty.name + ")");
    case "array":
      throw typeError("cannot assign an array value to composite column (type " + ty.name + ")");
    default: // default
      throw engineError("syntax_error", "DEFAULT is not allowed inside ROW(...)");
  }
}

// coerceStringToArray parses a text array literal into an array Value against the element ColType
// via array_in (spec/design/array.md §7): each token is coerced to the element type (an unquoted
// NULL token → NULL element). A malformed literal is 22P02.
export function coerceStringToArray(s: string, elem: ColType): Value {
  const parsed: ArrayInResult = parseArrayLiteral(s);
  if (!parsed.ok) {
    if (parsed.err === "boundflip")
      throw arraySubscriptErr("upper bound cannot be less than lower bound");
    throw engineError("invalid_text_representation", "malformed array literal");
  }
  const vals = parsed.value.tokens.map((tok) => {
    if (tok === null) return nullValue();
    // Coerce the token to the element type (a scalar via the string-literal coercion, a composite
    // via record_in — array-of-composite, spec/design/array.md §12 AC1 / §7).
    return coerceArrayElementText(tok, elem);
  });
  return {
    kind: "array",
    dims: parsed.value.dims,
    lbounds: parsed.value.lbounds,
    elements: vals,
  };
}

// coerceArrayElementText coerces one array-element token to a Value against the element ColType (the
// array_in per-element step, spec/design/array.md §7): a scalar via the string-literal coercion, a
// composite via record_in (recursive — the array-of-composite quoting nests, §12 AC1). Self-contained
// over the resolved ColType (no catalog re-walk). A nested-array element would recurse, but
// array-of-array is not a jed type, so it is unreachable in v1.
export function coerceArrayElementText(tok: string, elem: ColType): Value {
  if (elem.kind === "composite") return coerceRecordTextToValue(tok, elem);
  if (elem.kind === "array") return coerceStringToArray(tok, elem.elem);
  // A range element is unreachable: array-of-range is not a storable jed type (R2), so an array
  // element ColType is never a range.
  if (elem.kind === "range")
    throw new Error("array-of-range is not a storable type (ranges.md §2)");
  const { node } = coerceStringLiteral(tok, elem.scalar, null, null);
  return rexprConstToValue(node);
}

// coerceRecordTextToValue is record_in over a self-contained composite ColType (the inverse of
// record_out): the token is the composite's own `(f1,f2,…)` text, tokenized by the shared
// parseRecordTokens and recursively coerced per field (a scalar field respects its decimal typmod).
// Mirrors coerceStringToComposite but produces a Value directly and walks ColType (no Engine). A
// bad shape / field count is 22P02.
export function coerceRecordTextToValue(
  text: string,
  ct: { kind: "composite"; name: string; fields: ColField[] },
): Value {
  const tokens = parseRecordTokens(text);
  if (tokens === null || tokens.length !== ct.fields.length) {
    throw engineError(
      "invalid_text_representation",
      `malformed record literal: "${text}" for type ${ct.name}`,
    );
  }
  const vals = tokens.map((tok, i) => {
    if (tok === null) return nullValue();
    const f = ct.fields[i]!;
    if (f.type.kind === "composite") return coerceRecordTextToValue(tok, f.type);
    if (f.type.kind === "array") return coerceStringToArray(tok, f.type.elem);
    // A composite range field is unreachable: CREATE TYPE rejects a range field (R2).
    if (f.type.kind === "range")
      throw new Error("a composite range field is rejected at CREATE TYPE (R2)");
    const { node } = coerceStringLiteral(tok, f.type.scalar, f.typmod, f.varcharLen);
    return rexprConstToValue(node);
  });
  return compositeValue(vals);
}

// rexprConstToValue extracts the Value from a constant RExpr (the const nodes coerceStringLiteral
// produces).
export function rexprConstToValue(e: RExpr): Value {
  switch (e.kind) {
    case "constNull":
      return nullValue();
    case "constInt":
      return intValue(e.value);
    case "constBool":
      return boolValue(e.value);
    case "constText":
      return textValue(e.value);
    case "constDecimal":
      return decimalValue(e.value);
    case "constBytea":
      return byteaValue(e.value);
    case "constUuid":
      return uuidValue(e.value);
    case "constTimestamp":
      return timestampValue(e.value);
    case "constTimestamptz":
      return timestamptzValue(e.value);
    case "constDate":
      return dateValue(e.value);
    case "constInterval":
      return intervalValue(e.value);
    case "constJson":
      return jsonValue(e.value);
    case "constJsonb":
      return jsonbValue(e.value);
    case "constJsonPath":
      return jsonPathValue(e.value);
    case "constFloat":
      return e.ty === "f32" ? float32Value(e.value) : float64Value(e.value);
    default:
      throw typeError("non-constant array element literal");
  }
}

export function overflow(ty: ScalarType): Error {
  return engineError(
    "numeric_value_out_of_range",
    "value out of range for type " + canonicalName(ty),
  ).withDataType(canonicalName(ty));
}

export function typeError(msg: string): Error {
  return engineError("datatype_mismatch", msg);
}

export const I64_MIN = -9223372036854775808n;

// evalExpr evaluates against a row, accruing cost into m, and returns a Value (a boolean
// for comparisons / connectives). Arithmetic throws 22003 on overflow and 22012 on a zero
// divisor; NULL propagates through arithmetic; the connectives are Kleene; IS NULL is
// always definite.
//
// Cost: each INTERIOR node charges operator_eval once, pre-order (the node, then its
// operands LHS-before-RHS — JS evaluates arguments left-to-right); leaf nodes
// (column/constants) charge nothing. Both operands are always evaluated — there is no
// short-circuit, so the count never depends on operand values (spec/design/cost.md §3).
// inMembership is three-valued `lhs IN (list)` membership (spec/design/grammar.md §26), charging
// one operator_eval per element compared. An EMPTY list is `negated` (x IN () = FALSE, x NOT IN ()
// = TRUE) independent of lv. Otherwise: a positive match -> TRUE; else a NULL element (or NULL lv)
// -> NULL; else FALSE. NOT IN is the Kleene negation. Shared by the folded "inValues" node and the
// correlated "subquery"/in eval.
export function inMembership(lv: Value, list: Value[], negated: boolean, m: Meter): Value {
  if (list.length === 0) return { kind: "bool", value: negated };
  let anyMatch = false;
  let anyNull = false;
  for (const v of list) {
    m.charge(COSTS.operatorEval);
    // Each element comparison over a decimal pair charges its size-scaled decimal_work
    // (spec/design/cost.md §3 "decimal_work"), like a compare node.
    m.charge(COSTS.decimalWork * BigInt(decimalCmpWork(lv, v) - 1));
    m.guard();
    const t = eq3(lv, v);
    if (t === "true") anyMatch = true;
    else if (t === "unknown") anyNull = true;
  }
  const inVal: Value = anyMatch
    ? { kind: "bool", value: true }
    : anyNull
      ? nullValue()
      : { kind: "bool", value: false };
  return negated ? boolNot(inVal) : inVal;
}

// dateMidnightMicros returns midnight (00:00:00) of a date as timestamp microseconds, preserving
// the ±infinity sentinels. A finite date whose midnight instant overflows the i64-µs timestamp
// range traps 22008 (jed's date range is wider than the timestamp range — date.md §1). bigint
// never overflows, so the i64 range is checked explicitly (mirroring Rust's checked_mul / Go's
// mul64); a finite day count cannot equal a sentinel (i64 min/max are not multiples of a day).
export function dateMidnightMicros(d: bigint): bigint {
  const MICROS_PER_DAY = 86_400n * 1_000_000n;
  if (d === DATE_POS_INFINITY) return POS_INFINITY;
  if (d === DATE_NEG_INFINITY) return NEG_INFINITY;
  const mc = d * MICROS_PER_DAY;
  if (mc < NEG_INFINITY || mc > POS_INFINITY)
    throw engineError("datetime_field_overflow", "date out of range");
  return mc;
}

// evalDateArith evaluates a date arithmetic node (spec/design/date.md §6): date ± int → date
// (shift the i32 day count; ±infinity unchanged; a finite result beyond the i32 day range or onto a
// reserved sentinel traps 22008), date − date → i32 (days between; an ±infinity operand traps
// 22008; a difference beyond i32 traps 22008), and date ± interval → timestamp (the date widens to
// midnight, then the timestamp ± interval calendar shift). The resolver guarantees a Date operand
// is present and settled `result`. Day counts are bigint (the uniform-integer discipline).
export function evalDateArith(op: BinaryOp, a: Value, b: Value, result: ScalarType): Value {
  // date ± interval → timestamp: widen the date to midnight micros, then the calendar shift.
  if (isTimestamp(result)) {
    const d = a.kind === "date" ? a.days : (b as { days: bigint }).days;
    const iv = a.kind === "interval" ? a.iv : (b as { iv: Interval }).iv;
    return timestampValue(tsShift(dateMidnightMicros(d), iv, op === "sub"));
  }
  // date − date → i32 (days between); an ±infinity operand traps 22008.
  if (a.kind === "date" && b.kind === "date") {
    if (
      a.days === DATE_NEG_INFINITY ||
      a.days === DATE_POS_INFINITY ||
      b.days === DATE_NEG_INFINITY ||
      b.days === DATE_POS_INFINITY
    )
      throw engineError("datetime_field_overflow", "cannot subtract infinite dates");
    const diff = a.days - b.days;
    if (diff < DATE_NEG_INFINITY || diff > DATE_POS_INFINITY)
      throw engineError("datetime_field_overflow", "date out of range");
    return intValue(diff);
  }
  // date ± int → date: shift the day count; a ±infinity date stays the same sentinel.
  const d = a.kind === "date" ? a.days : (b as { days: bigint }).days;
  const n = a.kind === "int" ? a.int : (b as { int: bigint }).int;
  if (d === DATE_NEG_INFINITY || d === DATE_POS_INFINITY) return dateValue(d);
  const shifted = op === "sub" ? d - n : d + n;
  // A finite result must land strictly inside the i32 day range (the two extremes are the reserved
  // ±infinity sentinels — date.md §1); anything else traps 22008.
  if (shifted <= DATE_NEG_INFINITY || shifted >= DATE_POS_INFINITY)
    throw engineError("datetime_field_overflow", "date out of range");
  return dateValue(shifted);
}

// evalDateConvert evaluates a cross-family datetime cast (timezones.md §9.3) of the non-NULL value v
// to `to` (timestamp/timestamptz/date). The casts crossing the timestamptz boundary consult the
// session zone (charging timezone); the others are zone-free. ±infinity maps to the target's own
// sentinel. The (source family, to) pair is guaranteed cross-family by the resolver.
export function evalDateConvert(v: Value, to: ScalarType, env: EvalEnv, m: Meter): Value {
  const MICROS_PER_DAY = 86_400n * 1_000_000n;
  const microsToDate = (mc: bigint): Value => {
    if (mc === POS_INFINITY) return dateValue(DATE_POS_INFINITY);
    if (mc === NEG_INFINITY) return dateValue(DATE_NEG_INFINITY);
    const days = mc >= 0n ? mc / MICROS_PER_DAY : -((-mc + (MICROS_PER_DAY - 1n)) / MICROS_PER_DAY);
    return dateValue(days);
  };
  const dateToMicros = (d: bigint): bigint => {
    if (d === DATE_POS_INFINITY) return POS_INFINITY;
    if (d === DATE_NEG_INFINITY) return NEG_INFINITY;
    return d * MICROS_PER_DAY;
  };
  const isInf = (mc: bigint): boolean => mc === POS_INFINITY || mc === NEG_INFINITY;
  const zoneCharge = (): ZoneRef => {
    const zr = env.exec.session.timeZone;
    m.charge(COSTS.timezone);
    m.guard();
    return zr;
  };
  if (v.kind === "timestamp" && to === "date") return microsToDate(v.micros);
  if (v.kind === "date" && to === "timestamp") return timestampValue(dateToMicros(v.days));
  if (v.kind === "timestamptz" && to === "timestamp") {
    if (isInf(v.micros)) return timestampValue(v.micros);
    return timestampValue(instantToLocalMicros(zoneCharge(), v.micros));
  }
  if (v.kind === "timestamp" && to === "timestamptz") {
    if (isInf(v.micros)) return timestamptzValue(v.micros);
    return timestamptzValue(localToInstantMicros(zoneCharge(), v.micros));
  }
  if (v.kind === "timestamptz" && to === "date") {
    if (isInf(v.micros)) return microsToDate(v.micros);
    return microsToDate(instantToLocalMicros(zoneCharge(), v.micros));
  }
  if (v.kind === "date" && to === "timestamptz") {
    const mid = dateToMicros(v.days);
    if (isInf(mid)) return timestamptzValue(mid);
    return timestamptzValue(localToInstantMicros(zoneCharge(), mid));
  }
  throw new Error("unreachable: resolver restricts dateConvert to cross-family datetime casts");
}
