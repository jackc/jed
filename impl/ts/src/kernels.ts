// widthBucketNumeric is width_bucket over numerics: floor((operand−low)·count/(high−low)) + 1, with
// 0 below low / count+1 at-or-above high, and the reversed (low > high) range. The bucket is an EXACT
// truncated decimal quotient (all-positive in range, so trunc == floor). Returns the raw index (the
// caller range-checks it to int4). count > 0 is checked by the caller.
import { Decimal, EXP_LIMIT, decimalFromParts } from "./decimal.ts";
import {
  Engine,
  ParamTypes,
  Scope,
  resolve,
  resolvedTypeEqual,
  resolvedTypeOf,
  resolvedTypeOfCol,
  rtName,
  valueToRExpr,
  widthBucketErr,
} from "./executor.ts";
import {
  coerceStringToArray,
  overflow,
  rexprConstToValue,
  truncateToChars,
  typeError,
} from "./store.ts";
import type { BinaryOp, Expr } from "./ast.ts";
import type { AggCtx, RExpr, ResolvedType } from "./executor.ts";
import { engineError } from "./errors.ts";
import type { DecimalTypmod, ScalarType, Type } from "./types.ts";
import { emptyRangeValue, parseByteaHex, parseRecordTokens, parseUuid } from "./value.ts";
import { canonicalName, inRange, rank, roundToWidth, typeCanonicalName } from "./types.ts";
import type { Column, CompositeType } from "./catalog.ts";
import { elementScalar, finalizeRange, parseRangeText, rangeForElement } from "./range.ts";
import type { RangeDesc } from "./ranges_gen.ts";
import type { Value } from "./value.ts";
import { parseTimestamp, parseTimestamptz } from "./timestamp.ts";
import { parseDate } from "./date.ts";
import { parseFactorDecimal, parseInterval } from "./interval.ts";
import { jsonbIn, validateJson } from "./json.ts";
import { compile as jsonPathCompile, render as jsonPathRender } from "./jsonpath.ts";
export function widthBucketNumeric(
  op: Decimal,
  low: Decimal,
  high: Decimal,
  count: bigint,
): bigint {
  const cmpBounds = low.cmpValue(high);
  if (cmpBounds === 0) throw widthBucketErr("lower bound cannot equal upper bound");
  const countDec = Decimal.fromBigInt(count);
  const bucket = (hiNum: Decimal, loNum: Decimal, hiDen: Decimal, loDen: Decimal): bigint => {
    const num = hiNum.sub(loNum).mul(countDec);
    const den = hiDen.sub(loDen);
    const q = num.sub(num.rem(den)).div(den).roundToScale(0);
    const b = q.toBigIntRound();
    if (b === null) throw overflow("i32");
    return b + 1n;
  };
  if (cmpBounds < 0) {
    // ascending low < high
    if (op.cmpValue(low) < 0) return 0n;
    if (op.cmpValue(high) >= 0) return count + 1n;
    return bucket(op, low, high, low);
  }
  // descending low > high
  if (op.cmpValue(low) > 0) return 0n;
  if (op.cmpValue(high) <= 0) return count + 1n;
  return bucket(low, op, low, high);
}

// widthBucketFloat is width_bucket over f64: the same index in binary64 (a single correctly-rounded
// chain, so cross-core identical). A NaN operand/bound → 2201G; a non-finite bound → 2201G (the
// operand may be ±Inf, handled by the comparisons). Returns the raw index.
export function widthBucketFloat(op: number, low: number, high: number, count: bigint): bigint {
  if (Number.isNaN(op) || Number.isNaN(low) || Number.isNaN(high))
    throw widthBucketErr("operand, lower bound, and upper bound cannot be NaN");
  if (!Number.isFinite(low) || !Number.isFinite(high))
    throw widthBucketErr("lower and upper bounds must be finite");
  if (low === high) throw widthBucketErr("lower bound cannot equal upper bound");
  const cf = Number(count);
  if (low < high) {
    if (op < low) return 0n;
    if (op >= high) return count + 1n;
    return BigInt(Math.floor(((op - low) / (high - low)) * cf)) + 1n;
  }
  if (op > low) return 0n;
  if (op <= high) return count + 1n;
  return BigInt(Math.floor(((low - op) / (low - high)) * cf)) + 1n;
}

export function resolveOperandPair(
  scope: Scope,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { rl: RExpr; lt: ResolvedType; rr: RExpr; rt: ResolvedType } {
  const lhsLit = isAdaptableOperand(lhs);
  const rhsLit = isAdaptableOperand(rhs);
  let l: { node: RExpr; type: ResolvedType };
  let r: { node: RExpr; type: ResolvedType };
  if (lhsLit && rhsLit) {
    l = resolve(scope, lhs, "i64", ag, params);
    r = resolve(scope, rhs, "i64", ag, params);
  } else if (lhsLit) {
    r = resolve(scope, rhs, null, ag, params);
    l = resolve(scope, lhs, ctxOf(r.type), ag, params);
  } else if (rhsLit) {
    l = resolve(scope, lhs, null, ag, params);
    r = resolve(scope, rhs, ctxOf(l.type), ag, params);
  } else {
    l = resolve(scope, lhs, null, ag, params);
    r = resolve(scope, rhs, null, ag, params);
  }
  return { rl: l.node, lt: l.type, rr: r.node, rt: r.type };
}

// resolveIntPair resolves the two operands of an *arithmetic* operator: both must be
// integer (or NULL); a boolean or text operand is a 42804 type error.
// classifyComparable requires that a comparison operand pair is comparable
// (spec/types/compare.toml): both numeric (integer and/or decimal — the integer promotes to
// decimal), both text, or both boolean (NULL counts as either). A mixed numeric/text pair, or
// a boolean with a non-boolean, is a 42804 type error — comparison is overloaded across these
// families but never compares across them.
export function classifyComparable(lt: ResolvedType, rt: ResolvedType): void {
  // json is NOT comparable: PostgreSQL ships no btree/hash operator class for `json`, so jed matches
  // it (spec/design/json.md §5). ANY json comparison — even json × json, json × jsonb, or json × a
  // bare NULL — is 42883 (operator does not exist), distinct from the cross-family 42804 other types
  // use. Must precede the jsonb arms so json × jsonb is 42883.
  if (lt.kind === "json" || rt.kind === "json") {
    throw engineError("undefined_function", "operator does not exist: json is not comparable");
  }
  // jsonpath is likewise NOT comparable (PG ships no opclass — jsonpath.md §1): every comparison
  // is 42883.
  if (lt.kind === "jsonpath" || rt.kind === "jsonpath") {
    throw engineError("undefined_function", "operator does not exist: jsonpath is not comparable");
  }
  // jsonb IS comparable — PostgreSQL's total btree order (spec/design/json.md §5) — but only with
  // another jsonb (or a bare NULL). jsonb vs any other family is 42804 (jed's cross-family
  // convention, like uuid/bytea/range; a documented divergence from PG's 42883).
  const jsonbL = lt.kind === "jsonb";
  const jsonbR = rt.kind === "jsonb";
  if (jsonbL || jsonbR) {
    if ((jsonbL && jsonbR) || (jsonbL && rt.kind === "null") || (lt.kind === "null" && jsonbR)) {
      return;
    }
    throw typeError("cannot compare a jsonb value with a value of a different type");
  }
  // Range comparison is the PG range_cmp total order (spec/design/ranges.md §6). Two ranges are
  // comparable iff they are over the SAME element type — i32range × i32range only, never
  // i32range × i64range or i32range × i32 (no implicit cross-element range comparison this slice;
  // stricter than the int↔bigint scalar case, so the element types must be EQUAL, not merely
  // comparable). A bare NULL is always comparable (the comparison is unknown). Checked FIRST so a
  // range × array/composite pair reports the range message (matching the Rust arm order).
  const rangeL = lt.kind === "range";
  const rangeR = rt.kind === "range";
  if (rangeL && rangeR) {
    if (!resolvedTypeEqual(lt.elem, rt.elem)) {
      throw typeError("cannot compare ranges of different element types");
    }
    return;
  }
  if ((rangeL || rangeR) && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a range value with a value of a different type");
  }
  // Composite comparison is element-wise row comparison (spec/design/composite.md §5): two
  // composites are comparable iff they have the SAME field count and each corresponding field
  // pair is itself comparable (recursively — a nested composite recurses here, an anonymous
  // `ROW(…)` compares against a same-shape named type). A bare NULL is always comparable (the
  // comparison is unknown). A composite vs any non-composite, or a row-size mismatch, or an
  // incomparable field pair, is 42804 (S5; the old 0A000 narrowing is lifted).
  const compL = lt.kind === "composite";
  const compR = rt.kind === "composite";
  if (compL && compR) {
    if (lt.fields.length !== rt.fields.length) {
      throw typeError("cannot compare rows of different sizes");
    }
    for (let i = 0; i < lt.fields.length; i++) {
      classifyComparable(lt.fields[i]!.type, rt.fields[i]!.type);
    }
    return;
  }
  if ((compL || compR) && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a composite value with a value of a different type");
  }
  // Array comparison is element-wise (spec/design/array.md §5): two arrays are comparable iff their
  // element types are comparable (recursively). A bare NULL is always comparable; an array vs any
  // non-array is 42804.
  const arrL = lt.kind === "array";
  const arrR = rt.kind === "array";
  if (arrL && arrR) {
    classifyComparable(lt.elem, rt.elem);
    return;
  }
  if ((arrL || arrR) && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare an array value with a value of a different type");
  }
  // Boolean compares only with boolean (or NULL); boolean with a number/text is a mismatch.
  const boolL = lt.kind === "bool";
  const boolR = rt.kind === "bool";
  if (boolL !== boolR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a boolean value with a non-boolean value");
  }
  const lNum = lt.kind === "int" || lt.kind === "decimal";
  const rNum = rt.kind === "int" || rt.kind === "decimal";
  if ((lNum && rt.kind === "text") || (lt.kind === "text" && rNum)) {
    throw typeError("cannot compare a text value with a numeric value");
  }
  // float is a STRICT island (float.md §3/§6): float compares ONLY with float (either width — a
  // mixed-width pair promotes to f64) or NULL. float vs int/decimal/text/anything-else is a
  // 42804 — NOT comparable (PG promotes the other operand; jed requires an explicit cast, a
  // documented divergence). The pair is promoted to f64 in resolveBinary before eval.
  const floatL = lt.kind === "float";
  const floatR = rt.kind === "float";
  if (floatL !== floatR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a float value with a value of a different type");
  }
  // bytea compares only with bytea (or NULL); bytea with a number or text is a mismatch.
  const byteaL = lt.kind === "bytea";
  const byteaR = rt.kind === "bytea";
  if (byteaL !== byteaR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a bytea value with a non-bytea value");
  }
  // uuid compares only with uuid (or NULL); uuid with anything else is a mismatch.
  const uuidL = lt.kind === "uuid";
  const uuidR = rt.kind === "uuid";
  if (uuidL !== uuidR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a uuid value with a non-uuid value");
  }
  // timestamp / timestamptz compare only within their own family (or with NULL). A mixed
  // timestamp × timestamptz pair, or a datetime vs any other family, would need a zone, so it
  // is a 42804 type error (spec/design/timestamp.md §5).
  const tsL = lt.kind === "timestamp" || lt.kind === "timestamptz";
  const tsR = rt.kind === "timestamp" || rt.kind === "timestamptz";
  if ((tsL || tsR) && lt.kind !== rt.kind && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a timestamp value with a value of a different type");
  }
  // date compares only within its own family (or with NULL); date vs any other family — incl.
  // timestamp, which would need a cast — is a 42804 (date is a strict island, spec/design/date.md §4).
  const dateL = lt.kind === "date";
  const dateR = rt.kind === "date";
  if (dateL !== dateR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a date value with a value of a different type");
  }
  // interval compares only with itself (or NULL); interval vs any other family is a 42804.
  const ivL = lt.kind === "interval";
  const ivR = rt.kind === "interval";
  if (ivL !== ivR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare an interval value with a value of a different type");
  }
}

// isAdaptableOperand reports whether e is an adaptable operand — one that takes its type from its
// sibling: an integer, decimal, or string literal, or a bind parameter $N (spec/design/api.md §5,
// float.md §4). NULL and boolean literals do not take a sibling's context. A DECIMAL literal is
// adaptable so it can adopt a FLOAT sibling's context (`f = 1.5`, `f + 0.5` — float.md §4); in a
// non-float context the resolve decimal case ignores the context and stays decimal, so this widens
// adaptation ONLY for the float case (the int/decimal behavior is unchanged: a decimal literal
// against an int/decimal sibling still resolves to decimal).
export function isAdaptableOperand(e: Expr): boolean {
  if (e.kind === "param") return true;
  return (
    e.kind === "literal" &&
    (e.literal.kind === "int" || e.literal.kind === "decimal" || e.literal.kind === "text")
  );
}

// ctxOf returns the type a sibling operand offers an adaptable operand. For an integer literal
// this is the integer width it adopts; for a string literal, bytea/uuid/text (so it can decode
// the hex/uuid input); a bind parameter additionally adopts a decimal/boolean sibling (a literal
// ignores those — its arm keeps i64/text — so widening the mapping is safe). Only a bare NULL
// offers no context (spec/design/api.md §5).
export function ctxOf(t: ResolvedType): ScalarType | null {
  if (t.kind === "int") return t.ty;
  // A float sibling offers its width so an integer/decimal literal adapts to a float context
  // (float.md §4): `f + 1.5` types `1.5` as the float width, `f = 2` types `2` as the float width.
  if (t.kind === "float") return t.ty;
  if (t.kind === "bytea") return "bytea";
  if (t.kind === "uuid") return "uuid";
  if (t.kind === "text") return "text";
  if (t.kind === "bool") return "boolean";
  if (t.kind === "decimal") return "decimal";
  if (t.kind === "timestamp") return "timestamp";
  if (t.kind === "timestamptz") return "timestamptz";
  if (t.kind === "interval") return "interval";
  if (t.kind === "date") return "date";
  // A json/jsonb/jsonpath sibling offers its type so a string literal parses as that type.
  if (t.kind === "json") return "json";
  if (t.kind === "jsonb") return "jsonb";
  if (t.kind === "jsonpath") return "jsonpath";
  return null;
}

// intTypeOf returns the integer type of t (for promotion), or null.
export function intTypeOf(t: ResolvedType): ScalarType | null {
  return t.kind === "int" ? t.ty : null;
}

// decodeByteaLiteral decodes a single-quoted literal's content as a bytea value via the hex
// input form (parseByteaHex), mapping malformed hex to a 22P02 (invalid_text_representation).
// Used when a string literal adapts to a bytea context (types.md §6/§13); the trap is
// deterministic and fires at resolve time, before any scan.
export function decodeByteaLiteral(str: string): Uint8Array {
  const r = parseByteaHex(str);
  if ("error" in r) {
    throw engineError(
      "invalid_text_representation",
      "invalid input syntax for type bytea: " + r.error,
    );
  }
  return r.bytes;
}

// decodeUuidLiteral decodes a single-quoted literal's content as a uuid value via the
// PG-flexible input (parseUuid), mapping malformed input to a 22P02. Used when a string literal
// adapts to a uuid context (types.md §6/§14); deterministic, fires at resolve before any scan.
export function decodeUuidLiteral(str: string): Uint8Array {
  const r = parseUuid(str);
  if ("error" in r) {
    throw engineError(
      "invalid_text_representation",
      "invalid input syntax for type uuid: " + r.error,
    );
  }
  return r.bytes;
}

// LIT_WS is the ASCII whitespace set trimmed by the int/decimal/bool string coercions — EXACTLY
// Rust's is_ascii_whitespace (space, tab, LF, FF, CR; NO vertical tab), so the three cores trim
// byte-identically (a §8 determinism surface — JS's Unicode-aware String.trim would diverge).
export const LIT_WS = /^[ \t\n\f\r]+|[ \t\n\f\r]+$/g;
export const trimLit = (s: string): string => s.replace(LIT_WS, "");
export const allAsciiDigits = (s: string): boolean => /^[0-9]+$/.test(s);

// floatFromDecimalLiteral converts an untyped decimal/integer literal adapting to a float context
// into a float constant (float.md §4): the nearest binary64 to the exact decimal value (round-
// ties-to-even — JS Number(...) of the canonical decimal string is exactly the IEEE conversion),
// then Math.fround if the context width is f32. The exact-decimal cap-check is NOT applied: a
// literal adapting to a float column is a float value, not a stored decimal. A magnitude beyond the
// binary64 range becomes ±Infinity here — but a finite literal is meant, so an out-of-range literal
// traps 22003 (the finite-overflow rule, §3) rather than silently yielding Infinity.
export function floatFromDecimalLiteral(
  d: Decimal,
  ty: ScalarType,
): { node: RExpr; type: ResolvedType } {
  const exact = Number(d.render());
  if (!Number.isFinite(exact)) throw overflow(ty);
  const n = roundToWidth(ty, exact);
  if (!Number.isFinite(n)) throw overflow(ty); // f32 rounding pushed a finite double to ±Inf
  return {
    node: { kind: "constFloat", ty, value: n },
    type: { kind: "float", ty },
  };
}

// coerceStringToRangeExpr coerces a range text literal to a constant range expression
// ('[1,5)'::i32range / i32range '[1,5)'): parse, coerce each bound to the element type, then
// canonicalize (spec/design/ranges.md §4/§5). Folds to a constRange. 22P02 malformed / 22000
// lower>upper / 22003 canonicalize overflow.
// resolveContainerAssign resolves an UPDATE assignment RHS against a RANGE or ARRAY column (the
// caller has already rejected composite — 0A000). Mirrors INSERT's value adaptation (ranges.md §5 /
// array.md §7): a bare string literal adapts to the container via range_in / array_in, a bare NULL
// is the typed NULL, and any other expression must resolve to the SAME container type (matching
// element) else 42804. A top-level $N parameter is deferred (0A000) — INSERT's param-to-container
// handling is special and not generalized to the assignment RHS yet.
export function resolveContainerAssign(
  scope: Scope,
  col: Column,
  e: Expr,
  ag: AggCtx,
  params: ParamTypes,
): RExpr {
  const colRT = resolvedTypeOfCol(col.type, scope.catalog);
  // A bare string literal adapts to the container context (the same string-adapts-to-context rule
  // the cast and INSERT VALUES paths use).
  if (e.kind === "literal" && e.literal.kind === "text") {
    if (col.type.kind === "range") {
      if (col.type.elem.kind !== "scalar")
        throw new Error("a range element is always a scalar (ranges.md §2)");
      const desc = rangeForElement(col.type.elem.scalar);
      if (desc === undefined) throw new Error("a range column's element always has a range type");
      return coerceStringToRangeExpr(e.literal.text, desc).node;
    }
    // array
    const elem = (col.type as { kind: "array"; elem: Type }).elem;
    return valueToRExpr(coerceStringToArray(e.literal.text, scope.catalog.colTypeOf(elem)));
  }
  if (e.kind === "literal" && e.literal.kind === "null") {
    return { kind: "constNull" };
  }
  if (e.kind === "param") {
    const kind = col.type.kind === "array" ? "array" : "range";
    throw engineError(
      "feature_not_supported",
      `updating ${kind} column ${col.name} from a parameter is not supported yet`,
    );
  }
  // For an array column over a SCALAR element, pass the element type as the hint so a bare
  // ARRAY[1,2] constructor adapts its literal elements to the column's element type (the same
  // adaptation `col = ARRAY[…]` uses — without it, bare int literals would type as i64 and miss a
  // narrower i32[]/i16[] column). A range gets no scalar hint (its bare-literal form was handled
  // above; other forms self-describe their element).
  let hint: ScalarType | null = null;
  if (col.type.kind === "array" && col.type.elem.kind === "scalar") hint = col.type.elem.scalar;
  const { node, type } = resolve(scope, e, hint, ag, params);
  if (type.kind === "null") return node; // a NULL-typed expression (e.g. a CASE that may be NULL)
  // Ranges/arrays are assignable only over equal element types (resolvedTypeEqual compares the
  // element recursively), matching the comparison rule (ranges.md §6 / array.md §5).
  if (!resolvedTypeEqual(type, colRT)) {
    throw typeError(
      `column ${col.name} is of type ${typeCanonicalName(col.type)} but expression is of type ${rtName(type)}`,
    );
  }
  return node;
}

export function coerceStringToRangeExpr(
  text: string,
  desc: RangeDesc,
): { node: RExpr; type: ResolvedType } {
  const val = coerceStringToRange(text, desc);
  const elemRt = resolvedTypeOf(elementScalar(desc));
  return {
    node: { kind: "constRange", value: val },
    type: { kind: "range", elem: elemRt },
  };
}

export function coerceStringToRange(text: string, desc: RangeDesc): Value {
  const parsed = parseRangeText(text);
  if (parsed.empty) return emptyRangeValue();
  const elem = elementScalar(desc);
  const coerceBound = (b: string | null): Value | null => {
    if (b === null) return null;
    const { node } = coerceStringLiteral(b, elem, null, null);
    return rexprConstToValue(node);
  };
  const lower = coerceBound(parsed.lower);
  const upper = coerceBound(parsed.upper);
  return finalizeRange(desc, lower, upper, parsed.lowerInc, parsed.upperInc);
}

// coerceStringLiteral coerces a string literal's content to the named scalar target at resolve —
// the shared engine of the `type 'string'` typed literal and CAST(<string literal> AS target)
// (spec/design/grammar.md §36, types.md §5). Every scalar is reachable: the string-native types
// parse by their own input, text is identity, and int/decimal/boolean are the cast from text
// admitted only for a literal operand. 22P02 malformed / 22003 out of range / the type's parse
// code. typmod (decimal only) re-scales the result.
export function coerceStringLiteral(
  s: string,
  target: ScalarType,
  typmod: DecimalTypmod | null,
  varcharLen: number | null,
): { node: RExpr; type: ResolvedType } {
  switch (target) {
    case "bytea":
      return {
        node: { kind: "constBytea", value: decodeByteaLiteral(s) },
        type: { kind: "bytea" },
      };
    case "uuid":
      return {
        node: { kind: "constUuid", value: decodeUuidLiteral(s) },
        type: { kind: "uuid" },
      };
    case "timestamp":
      return {
        node: { kind: "constTimestamp", value: parseTimestamp(s) },
        type: { kind: "timestamp" },
      };
    case "timestamptz":
      return {
        node: { kind: "constTimestamptz", value: parseTimestamptz(s) },
        type: { kind: "timestamptz" },
      };
    case "date":
      return {
        node: { kind: "constDate", value: parseDate(s) },
        type: { kind: "date" },
      };
    case "interval":
      return {
        node: { kind: "constInterval", value: parseInterval(s) },
        type: { kind: "interval" },
      };
    case "json":
      // `json '…'` / CAST('…' AS json) — validate well-formedness, store the bytes verbatim
      // (spec/design/json.md §4); malformed → 22P02.
      validateJson(s);
      return { node: { kind: "constJson", value: s }, type: { kind: "json" } };
    case "jsonb":
      // `jsonb '…'` / CAST('…' AS jsonb) — parse + canonicalize (numbers→decimal, keys deduped +
      // sorted — §2); malformed → 22P02.
      return { node: { kind: "constJsonb", value: jsonbIn(s) }, type: { kind: "jsonb" } };
    case "jsonpath":
      // `'…'::jsonpath` / `jsonpath '…'` — compile (P1a structural subset) + store the canonical
      // normalized text. Malformed → 42601; an unsupported (valid-PG) construct → 0A000.
      return {
        node: { kind: "constJsonPath", value: jsonPathRender(jsonPathCompile(s)) },
        type: { kind: "jsonpath" },
      };
    case "text":
      // text 'x' is identity — the string IS the value. A varchar(n) 'x' typed literal /
      // CAST('x' AS varchar(n)) silently truncates to n code points (the explicit-cast rule,
      // spec/design/types.md §15) — no 22001 at resolve.
      return {
        node: {
          kind: "constText",
          value: varcharLen !== null ? truncateToChars(s, varcharLen) : s,
        },
        type: { kind: "text" },
      };
    case "boolean":
      return {
        node: { kind: "constBool", value: parseBoolLiteral(s) },
        type: { kind: "bool" },
      };
    case "f32":
    case "f64": {
      const n = parseFloatLiteral(s, target);
      return {
        node: { kind: "constFloat", ty: target, value: n },
        type: { kind: "float", ty: target },
      };
    }
    case "decimal": {
      let d = parseDecimalLiteral(s);
      d = typmod !== null ? d.coerceToTypmod(typmod.precision, typmod.scale) : d.checkCap();
      return {
        node: { kind: "constDecimal", value: d },
        type: { kind: "decimal" },
      };
    }
    default: {
      // i16 / i32 / i64
      const n = parseIntLiteral(s, target);
      return {
        node: { kind: "constInt", value: n },
        type: { kind: "int", ty: target },
      };
    }
  }
}

// coerceStringToComposite coerces a string literal to a named composite via record_in
// (spec/design/composite.md §8) — the shared primitive behind `'(…)'::addr` and the `addr '(…)'`
// typed literal. It tokenizes the text (a malformed literal or a field-count mismatch is 22P02
// "malformed record literal: …"), then coerces each token to its field's declared type: a NULL token
// (unquoted-empty) becomes a typed NULL; a scalar field reuses the same string-literal coercion as a
// typed literal (its own parse errors surface — e.g. 22P02 for a non-integer); a nested composite
// field recurses. Folds to a `row` RExpr of the coerced const field nodes, typed as the named
// composite (the TS-idiomatic equivalent of the Rust `RExpr::Row` over `ResolvedType::Composite`).
export function coerceStringToComposite(
  text: string,
  ct: CompositeType,
  db: Engine,
): { node: RExpr; type: ResolvedType } {
  const malformed = (): Error =>
    engineError(
      "invalid_text_representation",
      `malformed record literal: "${text}" for type ${ct.name}`,
    );
  const tokens = parseRecordTokens(text);
  if (tokens === null || tokens.length !== ct.fields.length) throw malformed();
  const nodes: RExpr[] = [];
  const fieldTypes: { name: string; type: ResolvedType }[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const tok = tokens[i]!;
    const f = ct.fields[i]!;
    if (tok === null) {
      // A NULL field: a NULL value, typed by the field's declared type.
      nodes.push({ kind: "constNull" });
      fieldTypes.push({ name: f.name, type: resolvedTypeOfCol(f.type, db) });
    } else if (f.type.kind === "composite") {
      const nested = db.compositeType(f.type.name);
      if (nested === undefined)
        throw new Error("nested composite type resolved at CREATE TYPE / load");
      const { node, type } = coerceStringToComposite(tok, nested, db);
      nodes.push(node);
      fieldTypes.push({ name: f.name, type });
    } else if (f.type.kind === "array") {
      // An array-typed field (spec/design/array.md §12): the token is an array text literal,
      // coerced through array_in against the element type — the same path a bare `'{…}'::T[]` cast
      // uses, one level down. Folds to a constant array.
      const elemCol = db.colTypeOf(f.type.elem);
      const val = coerceStringToArray(tok, elemCol);
      nodes.push(valueToRExpr(val));
      fieldTypes.push({ name: f.name, type: resolvedTypeOfCol(f.type, db) });
    } else if (f.type.kind === "range") {
      // A range field cannot occur: CREATE TYPE rejects a range field (range columns are not
      // storable yet — R2).
      throw new Error("a composite range field is rejected at CREATE TYPE (R2)");
    } else {
      const { node, type } = coerceStringLiteral(tok, f.type.scalar, f.decimal, f.varcharLen);
      nodes.push(node);
      fieldTypes.push({ name: f.name, type });
    }
  }
  return {
    node: { kind: "row", fields: nodes },
    type: { kind: "composite", name: ct.name, fields: fieldTypes },
  };
}

// parseIntLiteral parses a string literal's content as a signed integer of type ty — the
// text→integer coercion for INTEGER '42' / CAST('42' AS int) (grammar.md §36). jed's OWN
// integer-literal grammar: trimmed ASCII whitespace, optional +/-, then ASCII decimal digits
// (NO hex/octal/binary or underscores — 22P02, a documented PG divergence). Out of range → 22003.
export function parseIntLiteral(s: string, ty: ScalarType): bigint {
  const invalid = (): Error =>
    engineError(
      "invalid_text_representation",
      `invalid input syntax for type ${canonicalName(ty)}: "${s}"`,
    );
  let t = trimLit(s);
  let neg = false;
  if (t.startsWith("-")) {
    neg = true;
    t = t.slice(1);
  } else if (t.startsWith("+")) {
    t = t.slice(1);
  }
  if (t === "" || !allAsciiDigits(t)) throw invalid();
  // BigInt holds an arbitrary-length digit run; range is checked below (out of range → 22003).
  const v = neg ? -BigInt(t) : BigInt(t);
  if (!inRange(ty, v)) throw overflow(ty);
  return v;
}

// parseDecimalLiteral parses a string literal's content as a decimal — the text→decimal coercion
// for NUMERIC '1.5' / CAST('1.5' AS numeric) (grammar.md §36). jed's OWN decimal-literal grammar:
// trimmed ASCII whitespace, optional sign, ASCII digits with at most one '.' and a digit on at
// least one side, plus optional scientific e-notation (numeric '1.5e3' → 1500) — built into the SAME
// (digits, scale) the lexer feeds Decimal.fromDigitsScale (via the shared decimalFromParts), so
// NUMERIC 'x' is byte-identical to writing x. NO NaN / Infinity and no hex/underscore (22P02).
// Caller applies typmod / cap-check.
export function parseDecimalLiteral(s: string): Decimal {
  const invalid = (): Error =>
    engineError("invalid_text_representation", `invalid input syntax for type numeric: "${s}"`);
  let t = trimLit(s);
  let neg = false;
  if (t.startsWith("-")) {
    neg = true;
    t = t.slice(1);
  } else if (t.startsWith("+")) {
    t = t.slice(1);
  }
  // Split off an optional exponent. Unlike the lexer (which leaves a bare e for the next token), an
  // isolated string must be a COMPLETE numeric, so an e with no [+-]?digit+ after it is malformed
  // (22P02), matching PG's numeric_in.
  let mantissa = t;
  let exp: number | null = null;
  const ei = t.search(/[eE]/);
  if (ei >= 0) {
    mantissa = t.slice(0, ei);
    let e = t.slice(ei + 1);
    let eneg = false;
    if (e.startsWith("-")) {
      eneg = true;
      e = e.slice(1);
    } else if (e.startsWith("+")) {
      e = e.slice(1);
    }
    if (e === "" || !allAsciiDigits(e)) {
      throw invalid();
    }
    // Clamp the magnitude to EXP_LIMIT while accumulating (bounds the coefficient the shared
    // builder may materialize).
    let v = 0;
    for (let k = 0; k < e.length; k++) {
      if (v < EXP_LIMIT) {
        v = v * 10 + (e.charCodeAt(k) - 48);
        if (v > EXP_LIMIT) v = EXP_LIMIT;
      }
    }
    exp = eneg ? -v : v;
  }
  const dot = mantissa.indexOf(".");
  const intPart = dot < 0 ? mantissa : mantissa.slice(0, dot);
  const frac = dot < 0 ? "" : mantissa.slice(dot + 1);
  // A second '.' lands in frac (indexOf found the first); reject it.
  if (
    frac.includes(".") ||
    !(intPart === "" || allAsciiDigits(intPart)) ||
    !(frac === "" || allAsciiDigits(frac)) ||
    (intPart === "" && frac === "")
  ) {
    throw invalid();
  }
  const [digits, scale] = decimalFromParts(intPart, frac, exp);
  return Decimal.fromDigitsScale(neg, digits, scale);
}

// parseBoolLiteral parses a string literal's content as a boolean — the text→boolean coercion for
// BOOLEAN 'true' / CAST('t' AS boolean) (grammar.md §36). Matches PostgreSQL's boolin: trimmed
// ASCII whitespace, case-insensitive; t/tr/tru/true, y/ye/yes, on, 1 → true and f/fa/fal/fals/
// false, n/no, off, 0 → false; anything else 22P02.
export function parseBoolLiteral(s: string): boolean {
  switch (trimLit(s).toLowerCase()) {
    case "t":
    case "tr":
    case "tru":
    case "true":
    case "y":
    case "ye":
    case "yes":
    case "on":
    case "1":
      return true;
    case "f":
    case "fa":
    case "fal":
    case "fals":
    case "false":
    case "n":
    case "no":
    case "off":
    case "0":
      return false;
    default:
      throw engineError(
        "invalid_text_representation",
        `invalid input syntax for type boolean: "${s}"`,
      );
  }
}

// FLOAT_GRAMMAR is jed's f64 string-input grammar (float.md §4 — PG's float8in subset): an
// optional sign, then either a finite decimal (digits with an optional point and optional
// e-notation) or one of the special words. It is validated explicitly — NOT via parseFloat, which
// is too lenient (it accepts "1.5xyz", leading junk after trim, etc.). Anchored to the whole
// (trimmed) string so trailing junk is rejected → 22P02.
export const FLOAT_FINITE = /^[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$/;

// parseFloatLiteral parses a string literal's content as a float of type ty — the text→float
// coercion for `float '1.5'` / `real '1e10'` / CAST('Infinity' AS f64) (float.md §4). Grammar:
// trimmed ASCII whitespace (the shared LIT_WS), optional sign, finite decimal with optional point
// and e-notation, OR the case-insensitive specials Infinity/+Infinity/-Infinity/inf/+inf/-inf/NaN.
// Malformed input → 22P02; a finite value outside the binary64 range → 22003. For f32 the
// parsed binary64 is Math.fround'd; a finite value that frounds to ±Inf (beyond binary32 range)
// also traps 22003. NaN/±Infinity are first-class here (they enter ONLY via this path, casts, or
// stored values — float.md §3).
export function parseFloatLiteral(s: string, ty: ScalarType): number {
  const invalid = (): Error =>
    engineError(
      "invalid_text_representation",
      `invalid input syntax for type ${canonicalName(ty)}: "${s}"`,
    );
  const t = trimLit(s);
  // Special words (case-insensitive), with an optional leading sign on the infinities.
  const lower = t.toLowerCase();
  let special: number | undefined;
  switch (lower) {
    case "nan":
      special = NaN;
      break;
    case "infinity":
    case "+infinity":
    case "inf":
    case "+inf":
      special = Infinity;
      break;
    case "-infinity":
    case "-inf":
      special = -Infinity;
      break;
  }
  if (special !== undefined) return ty === "f32" ? Math.fround(special) : special;
  if (!FLOAT_GRAMMAR_OK(t)) throw invalid();
  // Number(...) does the IEEE-correct decimal→binary64 conversion (round-ties-to-even). The grammar
  // already rejected junk, so a NaN here would only come from an empty/degenerate string the regex
  // also rejects; guard anyway.
  const d = Number(t);
  if (Number.isNaN(d)) throw invalid();
  // A finite literal that overflows the binary64 range parses to ±Infinity — trap 22003 rather than
  // yield Infinity (Infinity is input-only via the special words above, not via a finite literal).
  if (!Number.isFinite(d)) throw overflow(ty);
  const n = ty === "f32" ? Math.fround(d) : d;
  if (!Number.isFinite(n)) throw overflow(ty); // finite double beyond binary32 range
  return n;
}

// FLOAT_GRAMMAR_OK tests the finite-decimal grammar (a named wrapper so the regex's role is legible).
export function FLOAT_GRAMMAR_OK(t: string): boolean {
  return FLOAT_FINITE.test(t);
}

// widenFloatTo wraps a float operand in an explicit widening cast when its width is narrower than
// the target (f32 → f64 is lossless — float.md §2), so a mixed-width float arithmetic /
// comparison node sees both sides at one width. Identity when from === to. Implemented as a `cast`
// RExpr (the evaluator's evalCast handles float→float widening), so no new node kind is needed.
export function widenFloatTo(node: RExpr, from: ScalarType, to: ScalarType): RExpr {
  return from === to
    ? node
    : { kind: "cast", target: to, typmod: null, varcharLen: null, operand: node };
}

// promote is the promotion-tower result type of two arithmetic operands: the
// higher-ranked integer type, or i64 when both are untyped NULLs.
export function promote(a: ResolvedType, b: ResolvedType): ScalarType {
  const ax = intTypeOf(a);
  const bx = intTypeOf(b);
  if (ax !== null && bx !== null) return rank(ax) >= rank(bx) ? ax : bx;
  if (ax !== null) return ax;
  if (bx !== null) return bx;
  return "i64";
}

// requireNumericOperand requires that an arithmetic operand is numeric (integer or decimal,
// or NULL); a boolean or text operand is a 42804 type error.
export function requireNumericOperand(t: ResolvedType): void {
  if (
    t.kind === "bool" ||
    t.kind === "text" ||
    t.kind === "bytea" ||
    t.kind === "uuid" ||
    t.kind === "timestamp" ||
    t.kind === "timestamptz" ||
    t.kind === "interval" ||
    t.kind === "date" ||
    t.kind === "json" ||
    t.kind === "jsonb" ||
    t.kind === "jsonpath" ||
    // A range/composite/array operand is non-numeric (range arithmetic + * - lands in RF4).
    t.kind === "range" ||
    t.kind === "composite" ||
    t.kind === "array" ||
    // float is a strict island — it never mixes with int/decimal arithmetic (the both-float case
    // is handled before this; reaching here with a float means a cross-family int/decimal ⊕ float
    // pair → 42804, float.md §6).
    t.kind === "float"
  ) {
    throw typeError("arithmetic operators require numeric operands");
  }
}

// temporalArithResult gives the result type of a temporal +/- (spec/design/interval.md §5), or
// undefined when neither operand is temporal (then arithmetic falls through to the numeric path).
// A temporal operand in an unsupported combination throws 42804. A NULL operand adopts the other
// side's temporal type (so `timestamp ± NULL` types as timestamp and evaluates to NULL).
export type RtKind = ResolvedType["kind"];

// intervalScaleResult gives the result type of an interval ×÷ number (spec/design/interval.md §5):
// interval * number, number * interval (commute), interval / number → interval. undefined when no
// interval is involved (or the op is not * / /). number / interval and interval × interval return
// undefined and fall to the ±-only temporal rule (which reports the 42804).
export function intervalScaleResult(op: BinaryOp, lt: RtKind, rt: RtKind): ScalarType | undefined {
  const lIv = lt === "interval";
  const rIv = rt === "interval";
  if (!lIv && !rIv) return undefined;
  const numeric = (k: RtKind) => k === "int" || k === "decimal" || k === "null";
  if (op === "mul" && ((lIv && numeric(rt)) || (rIv && numeric(lt)))) return "interval";
  if (op === "div" && lIv && numeric(rt)) return "interval";
  return undefined;
}

// factorToFraction returns a numeric factor value as an exact fraction [num, den] with den > 0.
export function factorToFraction(v: Value): [bigint, bigint] {
  if (v.kind === "int") return [v.int, 1n];
  if (v.kind === "decimal") return parseFactorDecimal(v.dec.render());
  throw typeError("internal: non-numeric interval-scale factor");
}

export function temporalArithResult(op: BinaryOp, lt: RtKind, rt: RtKind): ScalarType | undefined {
  const temporal = (k: RtKind) => k === "interval" || k === "timestamp" || k === "timestamptz";
  if (!temporal(lt) && !temporal(rt)) return undefined;
  const l = lt === "null" ? rt : lt;
  const r = rt === "null" ? lt : rt;
  if ((op === "add" || op === "sub") && l === "interval" && r === "interval") return "interval";
  if (
    op === "add" &&
    ((l === "timestamp" && r === "interval") || (l === "interval" && r === "timestamp"))
  )
    return "timestamp";
  if (op === "sub" && l === "timestamp" && r === "interval") return "timestamp";
  if (
    op === "add" &&
    ((l === "timestamptz" && r === "interval") || (l === "interval" && r === "timestamptz"))
  )
    return "timestamptz";
  if (op === "sub" && l === "timestamptz" && r === "interval") return "timestamptz";
  if (
    op === "sub" &&
    ((l === "timestamp" && r === "timestamp") || (l === "timestamptz" && r === "timestamptz"))
  )
    return "interval";
  throw typeError("unsupported operand types for temporal arithmetic");
}

// dateArithResult settles the result type of a date arithmetic operator (spec/design/date.md §6):
// date ± integer → date, integer + date → date (Add commutes; an integer of any width — the family
// covers i16/i32/i64), date − date → i32 (the count of days between, PG's int4), and date ±
// interval → timestamp (the date widens to midnight, then the timestamp ± interval calendar shift —
// PG: date + interval is a timestamp, not a date). interval + date commutes (Add only); there is no
// integer − date nor interval − date. Any other combination involving a date is a 42804 (PG reports
// 42883; jed uses its datatype-mismatch code, like the interval rule). A bare untyped NULL partner
// is NOT adopted — date ± NULL is a 42804 (PG rejects the ambiguous form too).
export function dateArithResult(op: BinaryOp, lt: RtKind, rt: RtKind): ScalarType {
  if (
    (op === "add" && lt === "date" && rt === "int") ||
    (op === "add" && lt === "int" && rt === "date") ||
    (op === "sub" && lt === "date" && rt === "int")
  )
    return "date";
  if (op === "sub" && lt === "date" && rt === "date") return "i32";
  if (
    (op === "add" && lt === "date" && rt === "interval") ||
    (op === "add" && lt === "interval" && rt === "date") ||
    (op === "sub" && lt === "date" && rt === "interval")
  )
    return "timestamp";
  throw typeError("unsupported operand types for date arithmetic");
}

export function requireBool(t: ResolvedType, msg: string): void {
  if (
    t.kind === "int" ||
    t.kind === "float" ||
    t.kind === "text" ||
    t.kind === "decimal" ||
    t.kind === "bytea" ||
    t.kind === "uuid" ||
    t.kind === "timestamp" ||
    t.kind === "timestamptz" ||
    t.kind === "interval" ||
    t.kind === "date" ||
    t.kind === "json" ||
    t.kind === "jsonb" ||
    t.kind === "jsonpath" ||
    t.kind === "range"
  ) {
    throw typeError(msg);
  }
}

// requireTextOrNull: LIKE requires both operands be text (or a bare NULL literal, which is
// comparable with anything and makes the result NULL at eval). A non-text operand is a 42804
// type error (spec/design/grammar.md §22).
export function requireTextOrNull(t: ResolvedType): void {
  if (t.kind !== "text" && t.kind !== "null") throw typeError("LIKE requires text operands");
}

// unifyArrayElementTypes unifies the element types of an ARRAY[...] constructor into one element
// type (spec/design/array.md §1). All-NULL → text (the PG unknown rule). All integer → the widest
// via the promotion tower (no runtime coercion — every integer is a bigint). Otherwise every element
// must be the SAME family — a cross-family mix (including int + decimal) is a documented 42804
// narrowing this slice (the representation-changing coercion is deferred with numeric(p,s)[]).
export function unifyArrayElementTypes(types: ResolvedType[]): ResolvedType {
  const nonNull = types.filter((t) => t.kind !== "null");
  if (nonNull.length === 0) return { kind: "text" };
  if (nonNull.every((t) => t.kind === "int")) {
    let acc = nonNull[0]!;
    for (const t of nonNull.slice(1)) acc = { kind: "int", ty: promote(acc, t) };
    return acc;
  }
  const first = nonNull[0]!;
  for (const t of nonNull.slice(1)) {
    if (t.kind !== first.kind) throw typeError("array elements must all be of the same type");
  }
  return first;
}

// arraySubscriptErr is a 2202E array-subscript error (spec/design/array.md §11).
export function arraySubscriptErr(detail: string): Error {
  return engineError("array_subscript_error", detail);
}

// countNulls counts the NULL (when wantNulls) or non-NULL values in vals — the shared kernel of
// num_nulls / num_nonnulls (spec/design/array-functions.md §12), over either the spread arguments or
// a VARIADIC array's flattened elements.
export function countNulls(vals: Value[], wantNulls: boolean): number {
  let n = 0;
  for (const v of vals) if ((v.kind === "null") === wantNulls) n++;
  return n;
}
