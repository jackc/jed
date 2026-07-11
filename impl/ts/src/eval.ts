import type { EvalEnv, RExpr, ScalarFuncName } from "./executor.ts";
import type { Row } from "./storage.ts";
import type { Meter } from "./cost.ts";
import type { ThreeValued, Value } from "./value.ts";
import { resolveUnfetchedSelf } from "./format.ts";
import {
  arrayOut,
  arrayValue,
  boolAnd,
  boolNot,
  boolOr,
  boolValue,
  byteaValue,
  compositeValue,
  dateValue,
  decimalValue,
  eq3,
  float32Value,
  float64Value,
  from3,
  gt3,
  intValue,
  intervalValue,
  isNullTest,
  jsonPathValue,
  jsonValue,
  jsonbValue,
  lt3,
  notDistinctFrom,
  nullValue,
  renderUuid,
  textValue,
  timestampValue,
  timestamptzValue,
  uuidValue,
} from "./value.ts";
import { COSTS } from "./costs.ts";
import { makeDate } from "./date.ts";
import {
  I64_MIN,
  applyJsonBehavior,
  buildNestedArray,
  chrText,
  coerceCaseValue,
  coerceDecimal,
  coerceStringToArray,
  collatedCmp,
  countNulls,
  decodeText,
  decodeUuidLiteral,
  elemJsonText,
  encodeBytea,
  dateClockValue,
  dateMidnightMicros,
  evalArrayFunc,
  evalDateArith,
  evalDateConvert,
  evalJsonSqlResult,
  evalJsonpath,
  evalRangeCtor,
  evalRangeFunc,
  evalRangeOp,
  evalRangeSetOp,
  evalSubscript,
  f64ToMicros,
  factorToFraction,
  gcdBigint,
  gcdDecimalValue,
  inMembership,
  initcapAscii,
  isSqljsonError,
  jsonArgNode,
  jsonPredKindMatches,
  lcmBigint,
  lcmDecimalValue,
  leftChars,
  minScaleOf,
  objectKeyNull,
  objectKeyText,
  overflow,
  padChars,
  parseBoolLiteral,
  parseDecimalLiteral,
  parseFloatLiteral,
  parseIntLiteral,
  quoteIdentText,
  quoteLiteralText,
  repeatText,
  rightChars,
  splitPart,
  substrChars,
  translateChars,
  trimChars,
  truncateToChars,
  typeError,
  utf8ByteLength,
  valueToNode,
  valueToOptTextArray,
  widthBucketErr,
  widthBucketFloat,
  widthBucketNumeric,
} from "./executor.ts";
import {
  inRange,
  isBool,
  isBytea,
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
import {
  intervalAdd,
  intervalNeg,
  intervalSub,
  makeInterval,
  mulByFraction,
  tsDiff,
  tsShift,
} from "./interval.ts";
import { Decimal, workDiv, workLinear, workMod, workMul } from "./decimal.ts";
import type { Interval } from "./interval.ts";
import { type EngineError, engineError } from "./errors.ts";
import { not3, or3, valueCmp } from "./window.ts";
import type { JsonMember, JsonNode } from "./json.ts";
import {
  arrayLength as jsonArrayLength,
  concat as jsonConcatKernel,
  contains as jsonContainsKernel,
  deleteIndex as jsonDeleteIndex,
  deleteKey as jsonDeleteKey,
  deleteKeys as jsonDeleteKeys,
  deletePath as jsonDeletePathKernel,
  getField,
  getIndex,
  getPath,
  hasDuplicateKeys as jsonHasDuplicateKeys,
  hasKey as jsonHasKeyKernel,
  insertPath as jsonInsertPathKernel,
  jsonCompactOut,
  jsonbIn,
  jsonbOut,
  makeObject as jsonMakeObject,
  nodeToText,
  parsePreservingJson,
  pretty as jsonPretty,
  setPath as jsonSetPathKernel,
  stripNulls as jsonStripNulls,
  typeofName as jsonTypeofName,
  validateJson,
} from "./json.ts";
import { foldCase, foldLowerSimple, loadedProperty } from "./collation.ts";
import {
  compileRegex,
  regexIsMatch,
  regexNinst,
  regexpCount,
  regexpMatch,
  regexpNthMatch,
  regexpReplace,
} from "./regex.ts";
import type { RegexProgram } from "./regex.ts";
import { NEG_INFINITY, POS_INFINITY, makeTimestamp } from "./timestamp.ts";
import {
  instantToLocalMicros,
  localToInstantMicros,
  offsetAtRef,
  resolveZone,
} from "./timezone.ts";
import { dateTruncInterval, dateTruncMicros, extractField } from "./datetime_fn.ts";
import type { ZoneRef } from "./timezone.ts";
import type { ExtractSrc } from "./datetime_fn.ts";
import { uuidExtractTimestampMicros, uuidExtractVersion } from "./uuid.ts";
import type { BinaryOp } from "./ast.ts";
import type { DecimalTypmod, ScalarType } from "./types.ts";
import { OPERATORS } from "./operators.ts";
export function evalExpr(e: RExpr, row: Row, env: EvalEnv, m: Meter): Value {
  // Enforce the cost ceiling before evaluating this node (CLAUDE.md §13). evalExpr recurses once
  // per expression node, so guarding here bounds a pathological expression to ~O(1) overshoot; it
  // is a no-op when no ceiling is set (spec/design/cost.md §6).
  m.guard();
  switch (e.kind) {
    case "column": {
      // A deferred large value the static touched set missed resolves ON TOUCH — the B4
      // demand-fault backstop (spec/design/bplus-reshape.md §5): deterministic rows, never a
      // NULL-fold; deliberately unmetered (§6).
      const v = row[e.index]!;
      return v.kind === "unfetched" ? resolveUnfetchedSelf(v.ref) : v;
    }
    case "outerColumn": {
      // A correlated reference: column `index` of the enclosing row `level` hops out (§26), with
      // the same demand-fault backstop as the column case.
      const v = env.outer[env.outer.length - e.level]![e.index]!;
      return v.kind === "unfetched" ? resolveUnfetchedSelf(v.ref) : v;
    }
    case "param":
      // The supplied value, already coerced to its inferred type by bindParams before execution
      // (spec/design/api.md §5).
      return env.params[e.index]!;
    case "constInt":
      return intValue(e.value);
    case "constFloat":
      // The value was already width-rounded at resolve (f32 frounded); rebuild the Value.
      return e.ty === "f32" ? float32Value(e.value) : float64Value(e.value);
    case "constBool":
      return { kind: "bool", value: e.value };
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
    case "constNull":
      return nullValue();
    case "row": {
      // A ROW(...) constructor — one operator_eval, then build the composite from the evaluated
      // fields (spec/design/composite.md §1, cost.md §9).
      m.charge(COSTS.operatorEval);
      const vals: Value[] = new Array(e.fields.length);
      for (let i = 0; i < e.fields.length; i++) vals[i] = evalExpr(e.fields[i]!, row, env, m);
      return compositeValue(vals);
    }
    case "array": {
      // An ARRAY[...] constructor — one operator_eval. A `nested` constructor stacks its sub-arrays
      // into one higher dimension (spec/design/array.md §4); otherwise a flat 1-D array.
      m.charge(COSTS.operatorEval);
      const elems: Value[] = new Array(e.elements.length);
      for (let i = 0; i < e.elements.length; i++) elems[i] = evalExpr(e.elements[i]!, row, env, m);
      return e.nested ? buildNestedArray(elems) : arrayValue(elems);
    }
    case "constArray":
      // A folded array constant (shape preserved) — return it directly.
      return e.value;
    case "constRange":
      // A folded range constant (already canonical) — return it directly.
      return e.value;
    case "field": {
      // Field selection — one operator_eval, then pull the resolved field ordinal out of the
      // evaluated composite. A whole-value-NULL composite yields NULL (PG); the index is in range
      // by construction (resolve fixed it against the static field list).
      m.charge(COSTS.operatorEval);
      const base = evalExpr(e.base, row, env, m);
      if (base.kind === "null") return nullValue();
      if (base.kind !== "composite")
        throw typeError("internal: field access on a non-composite value");
      return base.fields[e.index]!;
    }
    case "subscript": {
      // Array subscript `base[..][..]` — one operator_eval (spec/design/array.md §6). A NULL array
      // or any NULL subscript bound yields NULL; element access returns the element (or NULL), slice
      // access a (renumbered) sub-array. The per-element walk is internal (unmetered).
      m.charge(COSTS.operatorEval);
      return evalSubscript(e, row, env, m);
    }
    case "cast": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, env, m);
      if (v.kind === "null") return nullValue();
      const out = evalCast(v, e.target, e.typmod);
      // A varchar(n) cast target silently truncates the resulting text to n code points (the
      // explicit-cast rule, spec/design/types.md §15) — applied after any *→text conversion.
      if (e.varcharLen !== null && out.kind === "text") {
        return textValue(truncateToChars(out.text, e.varcharLen));
      }
      return out;
    }
    case "arrayCast": {
      // The three array-involving casts (spec/design/array.md §7): array → text (array_out),
      // runtime text → T[] (array_in per row), and element-wise array → array (each element through
      // the scalar cast). The node carries the cast's operator_eval charge (no new cost unit).
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, env, m);
      if (v.kind === "null") return nullValue();
      if (e.toElem === null) {
        // array → text: render via array_out (PG-byte-exact §7).
        return textValue(arrayOut(v as { dims: number[]; lbounds: number[]; elements: Value[] }));
      }
      if (v.kind === "text") {
        // runtime text → T[]: coerce the per-row string via array_in against the target element
        // ColType (22P02 malformed / 2202E inverted bound — the same as the '{…}'::T[] literal).
        return coerceStringToArray(v.text, e.toElem);
      }
      if (v.kind !== "array")
        throw new Error("an arrayCast operand is text or array (resolver-gated)");
      // element-wise array → other-element-array: every non-null element through the scalar element
      // cast (an array type takes no typmod, so a decimal target is unconstrained — typmod null);
      // the shape (dims/lbounds) is preserved and a NULL element stays NULL. The target element is
      // always a scalar (a same-element array is the identity, returned with no arrayCast node).
      if (e.toElem.kind !== "scalar") {
        throw new Error("an array→array element cast has a scalar target element");
      }
      const scalar = e.toElem.scalar;
      const elements = v.elements.map((el) =>
        el.kind === "null" ? nullValue() : evalCast(el, scalar, null),
      );
      return { kind: "array", dims: v.dims, lbounds: v.lbounds, elements };
    }
    case "neg": {
      m.charge(operatorCost("neg"));
      const v = evalExpr(e.operand, row, env, m);
      if (v.kind === "null") return nullValue();
      if (isInterval(e.result)) {
        if (v.kind !== "interval") throw typeError("internal: non-interval unary minus");
        return intervalValue(intervalNeg(v.iv));
      }
      if (isDecimal(e.result)) {
        return decimalValue(
          (v.kind === "int" ? Decimal.fromBigInt(v.int) : (v as { dec: Decimal }).dec).negate(),
        );
      }
      if (isFloat(e.result)) {
        // Negation flips the sign (no overflow); -NaN is NaN, -Inf is -Inf per IEEE. f32 stays
        // binary32 (negation never changes the width's representability, but fround keeps the path
        // uniform). float.md §5.
        if (v.kind !== "f32" && v.kind !== "f64")
          throw typeError("internal: non-float unary minus");
        return e.result === "f32" ? float32Value(-v.value) : float64Value(-v.value);
      }
      if (v.kind !== "int") throw typeError("internal: boolean unary minus");
      const n = -v.int;
      if (!inRange(e.result, n)) throw overflow(e.result);
      return intValue(n);
    }
    case "not": {
      m.charge(operatorCost("not"));
      return boolNot(evalExpr(e.operand, row, env, m));
    }
    case "arith": {
      m.charge(operatorCost(e.op));
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      if (a.kind === "null" || b.kind === "null") return nullValue();
      // Date arithmetic (spec/design/date.md §6): date ± int → date, date − date → i32, date ±
      // interval → timestamp. A Date operand is present iff this is date arithmetic (the resolver
      // settled e.result accordingly), so intercept it before the interval/timestamp/integer
      // dispatch below (which assume non-date operands).
      if (a.kind === "date" || b.kind === "date") return evalDateArith(e.op, a, b, e.result);
      if (isInterval(e.result) && (e.op === "mul" || e.op === "div")) {
        // interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5). Mul
        // commutes; Div is interval / number. A zero divisor traps 22012.
        const ivVal = a.kind === "interval" ? a : (b as { iv: Interval });
        const numVal = a.kind === "interval" ? b : a;
        let [fnum, fden] = factorToFraction(numVal);
        if (e.op === "div") {
          if (fnum === 0n) throw engineError("division_by_zero", "division by zero");
          // interval / number = interval * (den/num); keep fden > 0.
          [fnum, fden] = fnum < 0n ? [-fden, -fnum] : [fden, fnum];
        }
        return intervalValue(mulByFraction(ivVal.iv, fnum, fden));
      }
      if (isInterval(e.result)) {
        // interval ± interval → interval; timestamp[tz] − timestamp[tz] → interval (§5).
        if (a.kind === "interval" && b.kind === "interval") {
          return intervalValue(e.op === "add" ? intervalAdd(a.iv, b.iv) : intervalSub(a.iv, b.iv));
        }
        if (
          (a.kind === "timestamp" && b.kind === "timestamp") ||
          (a.kind === "timestamptz" && b.kind === "timestamptz")
        ) {
          return intervalValue(tsDiff(a.micros, b.micros));
        }
        throw typeError("internal: bad temporal-difference operands");
      }
      if (isTimestamp(e.result) || isTimestamptz(e.result)) {
        // timestamp[tz] ± interval → timestamp[tz] (calendar month-add; interval + ts commutes).
        let instant: bigint;
        let iv: Interval;
        if (a.kind === "interval") {
          iv = a.iv;
          instant = (b as { micros: bigint }).micros;
        } else {
          instant = (a as { micros: bigint }).micros;
          iv = (b as { iv: Interval }).iv;
        }
        const r = tsShift(instant, iv, e.op === "sub");
        return isTimestamptz(e.result) ? timestamptzValue(r) : timestampValue(r);
      }
      if (isDecimal(e.result)) {
        // Decimal arithmetic: widen any integer operand to decimal, then apply the op with
        // PG's scale rules (spec/design/decimal.md §4). The size-scaled decimal_work is
        // charged BEFORE the operation runs, so a cost ceiling aborts ahead of the limb
        // work (spec/design/cost.md §3 "decimal_work").
        const da = toDecimal(a);
        const db = toDecimal(b);
        m.charge(COSTS.decimalWork * BigInt(decimalArithWork(e.op, da, db) - 1));
        m.guard();
        return decimalValue(evalDecimalArith(e.op, da, db));
      }
      if (isFloat(e.result)) {
        // Float arithmetic: the resolver promoted both operands to e.result's width (mixed-width
        // pairs were cast to f64), so both are the same float kind here. One IEEE op per node
        // (no FMA fusion — structural in the tree walker, float.md §5).
        if ((a.kind !== "f32" && a.kind !== "f64") || (b.kind !== "f32" && b.kind !== "f64")) {
          throw typeError("internal: non-float arithmetic");
        }
        return evalFloatArith(e.op, a.value, b.value, e.result);
      }
      if (a.kind !== "int" || b.kind !== "int") throw typeError("internal: non-integer arithmetic");
      return evalArith(e.op, a.int, b.int, e.result);
    }
    case "compare": {
      m.charge(operatorCost(e.op));
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      // A decimal(-promotable) pair charges size-scaled decimal_work — once per node, even
      // where <=/>= decompose internally (spec/design/cost.md §3 "decimal_work").
      m.charge(COSTS.decimalWork * BigInt(decimalCmpWork(a, b) - 1));
      m.guard();
      // A collated ORDERING comparison (< <= > >=) over two non-NULL text values orders by the
      // collation's UCA sort key (spec/design/collation.md §7), charging the collate unit per code
      // point of each operand (cost.md "collate"). =/<> are byte-equality even under a deterministic
      // collation (§7), so they take the plain path and charge no collate. A NULL operand ⇒ Unknown
      // (no sort key). [...s] counts code points (NOT s.length — the UTF-16 trap, §8).
      if (
        e.collation !== null &&
        (e.op === "lt" || e.op === "gt" || e.op === "le" || e.op === "ge")
      ) {
        if (a.kind === "text" && b.kind === "text") {
          m.charge(COSTS.collate * BigInt([...a.text].length + [...b.text].length));
          m.guard();
          const c = collatedCmp(e.collation, a.text, b.text);
          let res: boolean;
          switch (e.op) {
            case "lt":
              res = c < 0;
              break;
            case "gt":
              res = c > 0;
              break;
            case "le":
              res = c <= 0;
              break;
            default: // "ge"
              res = c >= 0;
          }
          return { kind: "bool", value: res };
        }
        // Either operand NULL ⇒ Unknown (text comparison is three-valued).
        return { kind: "null" };
      }
      // Variable-length text/bytea comparison scans up to the shorter operand's length (code points
      // / bytes); charge varlen_compare × (W − 1) so the per-comparison length work an untrusted
      // join / correlated re-scan can amplify by fan-out is metered, not flat (spec/design/cost.md
      // §3 "varlen_compare"). Collated ORDERING already charged collate above and returned; this
      // covers =/<>, C/default-collation ordering, and all bytea.
      m.charge(COSTS.varlenCompare * BigInt(varlenCompareWork(a, b) - 1));
      m.guard();
      switch (e.op) {
        case "eq":
          return from3(eq3(a, b));
        case "ne":
          return from3(not3(eq3(a, b)));
        case "lt":
          return from3(lt3(a, b));
        case "gt":
          return from3(gt3(a, b));
        case "le":
          return from3(or3(lt3(a, b), eq3(a, b)));
        default: // "ge"
          return from3(or3(gt3(a, b), eq3(a, b)));
      }
    }
    case "and": {
      m.charge(operatorCost("and"));
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      return boolAnd(a, b);
    }
    case "or": {
      m.charge(operatorCost("or"));
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      return boolOr(a, b);
    }
    case "jsonGet": {
      // A jsonb accessor operator (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1). One
      // operator_eval; the operands charge their own. The operators are STRICT — a NULL base or
      // argument propagates to SQL NULL.
      m.charge(COSTS.operatorEval);
      const bv = evalExpr(e.base, row, env, m);
      const av = evalExpr(e.arg, row, env, m);
      m.guard();
      if (bv.kind === "null" || av.kind === "null") return nullValue();
      if (bv.kind !== "jsonb")
        throw new Error("resolver guarantees a jsonb base for an accessor operator");
      const node = bv.node;
      // Locate the accessed node: a key (text) / index (int) for `-> ->>`, or a text[] path for
      // `#> #>>`. A NULL element inside the path array misses (PG).
      let accessed: JsonNode | null;
      if (e.op === "arrow" || e.op === "arrowText") {
        if (av.kind === "text") {
          accessed = getField(node, av.text);
        } else if (av.kind === "int") {
          accessed = getIndex(node, av.int);
        } else {
          throw new Error("resolver guarantees a text/int arg for -> / ->>");
        }
      } else {
        // `#> #>>` — a text[] path.
        if (av.kind !== "array") throw new Error("resolver guarantees a text[] arg for #> / #>>");
        const steps: string[] = [];
        let nullStep = false;
        for (const el of av.elements) {
          if (el.kind === "null") {
            nullStep = true;
            break;
          }
          if (el.kind !== "text") throw new Error("a text[] path has text/NULL elements");
          steps.push(el.text);
        }
        accessed = nullStep ? null : getPath(node, steps);
      }
      // `-> #>` return the node as jsonb; `->> #>>` render it as text (a JSON null or a missing
      // access → SQL NULL).
      if (accessed === null) return nullValue();
      if (e.op === "arrow" || e.op === "hashArrow") return jsonbValue(accessed);
      const t = nodeToText(accessed);
      return t === null ? nullValue() : textValue(t);
    }
    case "jsonContains": {
      // `a @> b` jsonb deep containment (spec/design/json-sql-functions.md §1, J5). One
      // operator_eval; STRICT — a NULL operand yields SQL NULL.
      m.charge(COSTS.operatorEval);
      const av = evalExpr(e.a, row, env, m);
      const bv = evalExpr(e.b, row, env, m);
      m.guard();
      if (av.kind === "null" || bv.kind === "null") return nullValue();
      if (av.kind !== "jsonb" || bv.kind !== "jsonb")
        throw new Error("resolver guarantees jsonb operands for @> / <@");
      return { kind: "bool", value: jsonContainsKernel(av.node, bv.node) };
    }
    case "jsonHasKey": {
      // `jsonb ? text` / `?| text[]` / `?& text[]` key-existence (json-sql-functions.md §1, J5).
      // One operator_eval; STRICT — a NULL base or argument yields SQL NULL.
      m.charge(COSTS.operatorEval);
      const bv = evalExpr(e.base, row, env, m);
      const av = evalExpr(e.arg, row, env, m);
      m.guard();
      if (bv.kind === "null" || av.kind === "null") return nullValue();
      if (bv.kind !== "jsonb") throw new Error("resolver guarantees a jsonb base for ? / ?| / ?&");
      const node = bv.node;
      let result: boolean;
      if (e.hasKeyKind === "one") {
        if (av.kind !== "text") throw new Error("resolver guarantees a text arg for ?");
        result = jsonHasKeyKernel(node, av.text);
      } else {
        // `?|` / `?&` — a text[] arg. A NULL element never matches (PG): `?&` over an array with a
        // NULL is false; `?|` simply skips it.
        if (av.kind !== "array") throw new Error("resolver guarantees a text[] arg for ?| / ?&");
        const keys: string[] = [];
        let hasNull = false;
        for (const el of av.elements) {
          if (el.kind === "null") {
            hasNull = true;
            continue;
          }
          if (el.kind !== "text") throw new Error("a text[] arg has text/NULL elements");
          keys.push(el.text);
        }
        if (e.hasKeyKind === "any") {
          result = keys.some((k) => jsonHasKeyKernel(node, k));
        } else {
          // "all"
          result = !hasNull && keys.every((k) => jsonHasKeyKernel(node, k));
        }
      }
      return { kind: "bool", value: result };
    }
    case "jsonConcat": {
      // `a || b` jsonb concatenate / shallow-merge (json-sql-functions.md §1, J6). One
      // operator_eval; STRICT — a NULL operand yields SQL NULL.
      m.charge(COSTS.operatorEval);
      const av = evalExpr(e.a, row, env, m);
      const bv = evalExpr(e.b, row, env, m);
      m.guard();
      if (av.kind === "null" || bv.kind === "null") return nullValue();
      if (av.kind !== "jsonb" || bv.kind !== "jsonb")
        throw new Error("resolver guarantees jsonb operands for ||");
      return jsonbValue(jsonConcatKernel(av.node, bv.node));
    }
    case "jsonDelete": {
      // `jsonb - text|int|text[]` / `jsonb #- text[]` mutation deletes (json-sql-functions.md §1,
      // J6). One operator_eval; STRICT — a NULL base or argument yields SQL NULL.
      m.charge(COSTS.operatorEval);
      const bv = evalExpr(e.base, row, env, m);
      const av = evalExpr(e.arg, row, env, m);
      m.guard();
      if (bv.kind === "null" || av.kind === "null") return nullValue();
      if (bv.kind !== "jsonb") throw new Error("resolver guarantees a jsonb base for - / #-");
      const node = bv.node;
      // Extract a text[] argument's keys (a NULL element propagates to a NULL result, PG).
      const textArray = (v: Value): string[] | null => {
        if (v.kind !== "array") return null;
        const keys: string[] = [];
        for (const el of v.elements) {
          if (el.kind !== "text") return null; // a NULL element → NULL result
          keys.push(el.text);
        }
        return keys;
      };
      let result: JsonNode;
      switch (e.deleteKind) {
        case "key":
          if (av.kind !== "text") throw new Error("resolver guarantees a text arg for - key");
          result = jsonDeleteKey(node, av.text);
          break;
        case "index":
          if (av.kind !== "int") throw new Error("resolver guarantees an int arg for - index");
          result = jsonDeleteIndex(node, av.int);
          break;
        case "keys": {
          const keys = textArray(av);
          if (keys === null) return nullValue();
          result = jsonDeleteKeys(node, keys);
          break;
        }
        default: {
          // "path"
          const path = textArray(av);
          if (path === null) return nullValue();
          result = jsonDeletePathKernel(node, path);
          break;
        }
      }
      return jsonbValue(result);
    }
    case "isNull": {
      m.charge(COSTS.operatorEval);
      // PG's `IS [NOT] NULL` (spec/design/composite.md §5): for a composite the two are NOT
      // negations but the all-fields rule (one level deep, not recursive); a scalar follows the
      // ordinary rule. isNullTest folds both cases. Replaces the old `(v is null) !== negated`.
      const operand = evalExpr(e.operand, row, env, m);
      return { kind: "bool", value: isNullTest(operand, e.negated) };
    }
    case "isJson": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, env, m);
      let ok: boolean;
      switch (v.kind) {
        case "null":
          return nullValue(); // a NULL operand → NULL (never raises)
        // jsonb is always well-formed with unique keys; only the kind can fail.
        case "jsonb":
          ok = jsonPredKindMatches(v.node, e.jsonKind);
          break;
        // A string / json operand: parse (preserving duplicate keys); malformed → false.
        case "json":
        case "text": {
          let node: JsonNode;
          try {
            node = parsePreservingJson(v.text);
          } catch {
            ok = false;
            break;
          }
          ok =
            jsonPredKindMatches(node, e.jsonKind) && !(e.uniqueKeys && jsonHasDuplicateKeys(node));
          break;
        }
        default:
          throw new Error(
            "IS JSON: resolver restricts the operand to a string / json / jsonb value",
          );
      }
      return { kind: "bool", value: ok !== e.negated };
    }
    case "jsonCtor": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, env, m);
      if (v.kind === "null") return nullValue(); // STRICT
      if (v.kind !== "text") {
        throw new Error("BUG: resolver restricts JSON() to a text operand");
      }
      // Validate the string is well-formed JSON (22P02 on malformed — propagated), preserving
      // duplicate keys so the optional UNIQUE KEYS check (22030) can see them.
      const node = parsePreservingJson(v.text);
      if (e.uniqueKeys && jsonHasDuplicateKeys(node)) {
        throw engineError("duplicate_json_object_key_value", "duplicate JSON object key value");
      }
      // The result is the verbatim input text as a `json` value (PG).
      return jsonValue(v.text);
    }
    case "distinct": {
      m.charge(COSTS.operatorEval);
      const dl = evalExpr(e.lhs, row, env, m);
      const dr = evalExpr(e.rhs, row, env, m);
      // IS [NOT] DISTINCT FROM is a comparison: a decimal pair charges its size-scaled
      // decimal_work like "compare" (spec/design/cost.md §3 "decimal_work").
      m.charge(COSTS.decimalWork * BigInt(decimalCmpWork(dl, dr) - 1));
      m.guard();
      const same = notDistinctFrom(dl, dr);
      // negated carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks "are they
      // the same?"; IS DISTINCT FROM asks the opposite. Always a definite boolean — never
      // unknown (the null_safe discipline, functions.md §3).
      return { kind: "bool", value: same === e.negated };
    }
    case "like": {
      m.charge(COSTS.operatorEval);
      const subject = evalExpr(e.lhs, row, env, m);
      const pattern = evalExpr(e.rhs, row, env, m);
      // NULL propagates BEFORE the matcher runs, so a malformed pattern against a NULL operand
      // is still NULL, never 22025 (matches PG — grammar.md §22).
      if (subject.kind === "null" || pattern.kind === "null") return nullValue();
      if (subject.kind !== "text" || pattern.kind !== "text") {
        throw new Error("unreachable: resolver requires text LIKE operands");
      }
      let sub = subject.text;
      let pat = pattern.text;
      // ILIKE: simple-lowercase both sides under the engine casing regime (collation.md §16) before
      // matching — 1:1 folding so the matcher's _/length semantics survive.
      if (e.insensitive) {
        const prop = loadedProperty();
        sub = foldLowerSimple(sub, prop);
        pat = foldLowerSimple(pat, prop);
      }
      // negated carries NOT LIKE/ILIKE: matched !== negated flips for the NOT form.
      return { kind: "bool", value: likeMatch(sub, pat) !== e.negated };
    }
    case "regex": {
      m.charge(COSTS.operatorEval);
      const subject = evalExpr(e.lhs, row, env, m);
      const pattern = evalExpr(e.rhs, row, env, m);
      // NULL propagates BEFORE the matcher runs (regex.md §1) — a malformed pattern against a NULL
      // operand is still NULL, never 2201B.
      if (subject.kind === "null" || pattern.kind === "null") return nullValue();
      if (subject.kind !== "text" || pattern.kind !== "text") {
        throw new Error("unreachable: resolver requires text regex operands");
      }
      // ~* (insensitive): simple-lowercase the subject under the engine casing regime (collation.md
      // §16). The constant pattern was folded at resolve; a non-constant pattern is folded below.
      const prop = e.insensitive ? loadedProperty() : undefined;
      const sub = e.insensitive ? foldLowerSimple(subject.text, prop) : subject.text;
      const subjCps = Array.from(sub, (c) => c.codePointAt(0) as number);
      let matched: boolean;
      if (e.program !== null) {
        // Constant precompiled pattern: charge its regex_compile cost ONCE per statement execution
        // (on first eval), not per row (regex.md §5).
        if (!e.compileCharged) {
          e.compileCharged = true;
          m.charge(COSTS.regexCompile * BigInt(regexNinst(e.program)));
          m.guard();
        }
        matched = regexIsMatch(e.program, subjCps, m);
      } else {
        // Non-constant pattern: compile now (charging regex_compile) and run.
        const pat = e.insensitive ? foldLowerSimple(pattern.text, prop) : pattern.text;
        const prog = compileRegex(pat);
        m.charge(COSTS.regexCompile * BigInt(regexNinst(prog)));
        m.guard();
        matched = regexIsMatch(prog, subjCps, m);
      }
      // negated carries !~ / !~*: matched !== negated flips for the negated form.
      return { kind: "bool", value: matched !== e.negated };
    }
    case "regexFunc": {
      m.charge(COSTS.operatorEval);
      // STRICT: evaluate the args; any NULL short-circuits to NULL (regex.md §8).
      const vals: Value[] = [];
      for (const a of e.args) {
        const v = evalExpr(a, row, env, m);
        if (v.kind === "null") return nullValue();
        vals.push(v);
      }
      const text = (v: Value): string => {
        if (v.kind !== "text")
          throw new Error("unreachable: resolver requires text regexp_* operands");
        return v.text;
      };
      const int = (v: Value): number => {
        if (v.kind !== "int")
          throw new Error("unreachable: resolver requires integer regexp_* operands");
        return Number(v.int);
      };
      const source = text(vals[0]);
      const pattern = text(vals[1]);
      // Per-function argument layout (regex.md §8 / §8b); the numeric defaults match PG.
      let replacement = "";
      let flags = "";
      let start = 1;
      let nth = 1;
      let endoption = 0;
      let subexpr = 0;
      switch (e.func) {
        case "replace":
          replacement = text(vals[2]);
          if (vals[3]) flags = text(vals[3]);
          break;
        case "match":
        case "like":
          if (vals[2]) flags = text(vals[2]);
          break;
        case "count":
          if (vals[2]) start = int(vals[2]);
          if (vals[3]) flags = text(vals[3]);
          break;
        case "substr":
          if (vals[2]) start = int(vals[2]);
          if (vals[3]) nth = int(vals[3]);
          if (vals[4]) flags = text(vals[4]);
          if (vals[5]) subexpr = int(vals[5]);
          break;
        case "instr":
          if (vals[2]) start = int(vals[2]);
          if (vals[3]) nth = int(vals[3]);
          if (vals[4]) endoption = int(vals[4]);
          if (vals[5]) flags = text(vals[5]);
          if (vals[6]) subexpr = int(vals[6]);
          break;
      }
      // Numeric argument validation (regex.md §8b), BEFORE the pattern compiles (PG order: a bad
      // `start` beats a bad pattern). 22023 names the offending parameter.
      const badParam = (p: string, v: number): EngineError =>
        engineError("invalid_parameter_value", `invalid value for parameter "${p}": ${v}`);
      if (e.func === "count" || e.func === "substr" || e.func === "instr") {
        if (start < 1) throw badParam("start", start);
      }
      if (e.func === "substr" || e.func === "instr") {
        if (nth < 1) throw badParam("n", nth);
      }
      if (e.func === "instr" && endoption !== 0 && endoption !== 1)
        throw badParam("endoption", endoption);
      if ((e.func === "substr" || e.func === "instr") && subexpr < 0)
        throw badParam("subexpr", subexpr);
      // Validate flags: `i` (all), `g` (replace only); anything else is 2201B.
      for (const c of flags) {
        if (!(c === "i" || (c === "g" && e.func === "replace"))) {
          throw engineError(
            "invalid_regular_expression",
            `invalid regular expression: invalid option "${c}"`,
          );
        }
      }
      const insensitive = flags.includes("i");
      const global = flags.includes("g");
      // The original-case subject (for output/captures) and the matched subject (folded when
      // case-insensitive — same length, so offsets carry over, regex.md §8).
      const origCps = Array.from(source, (ch) => ch.codePointAt(0) as number);
      const prop = insensitive ? loadedProperty() : undefined;
      const matchCps = insensitive
        ? Array.from(foldLowerSimple(source, prop), (ch) => ch.codePointAt(0) as number)
        : origCps;
      let prog: RegexProgram;
      if (e.program !== null) {
        if (!e.compileCharged) {
          e.compileCharged = true;
          m.charge(COSTS.regexCompile * BigInt(regexNinst(e.program)));
          m.guard();
        }
        prog = e.program;
      } else {
        const pat = insensitive ? foldLowerSimple(pattern, prop) : pattern;
        prog = compileRegex(pat);
        m.charge(COSTS.regexCompile * BigInt(regexNinst(prog)));
        m.guard();
      }
      // 0-based search start; clamp to len+1 (a start past len+1 never enters the iteration loop →
      // 0 / NULL, the PG rule, regex.md §8b).
      const start0 = Math.min(start - 1, matchCps.length + 1);
      switch (e.func) {
        case "replace": {
          const repl = Array.from(replacement, (ch) => ch.codePointAt(0) as number);
          return { kind: "text", text: regexpReplace(prog, matchCps, origCps, repl, global, m) };
        }
        case "match": {
          const groups = regexpMatch(prog, matchCps, origCps, m);
          if (groups === null) return nullValue();
          return arrayValue(
            groups.map((g) => (g === null ? nullValue() : { kind: "text", text: g })),
          );
        }
        case "like":
          return boolValue(regexIsMatch(prog, matchCps, m));
        case "count":
          return intValue(BigInt(regexpCount(prog, matchCps, start0, m)));
        default: {
          // substr / instr — both find the N-th match's subexpr span.
          const saves = regexpNthMatch(prog, matchCps, start0, nth, m);
          const noMatch = (): Value => (e.func === "substr" ? nullValue() : intValue(0n));
          if (saves === null) return noMatch();
          // `subexpr` selects the whole match (0) or a capture group; out of range (> group count)
          // or a non-participating group (-1) → NULL / 0.
          const ng = saves.length / 2 - 1;
          if (subexpr > ng) return noMatch();
          const si = 2 * subexpr;
          const s2 = saves[si];
          const e2 = saves[si + 1];
          if (s2 < 0 || e2 < 0) return noMatch();
          if (e.func === "substr") {
            return { kind: "text", text: String.fromCodePoint(...origCps.slice(s2, e2)) };
          }
          // endoption 0 → first-char position, 1 → after-last-char (1-based).
          return intValue(BigInt(endoption === 0 ? s2 + 1 : e2 + 1));
        }
      }
    }
    case "casing": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.arg, row, env, m);
      if (v.kind === "null") return nullValue();
      if (v.kind !== "text") {
        throw new Error("unreachable: resolver requires text upper/lower operand");
      }
      return textValue(foldCase(v.text, e.upper, loadedProperty()));
    }
    case "atTimeZone": {
      m.charge(COSTS.operatorEval);
      const zv = evalExpr(e.zone, row, env, m);
      const vv = evalExpr(e.value, row, env, m);
      if (zv.kind === "null" || vv.kind === "null") return nullValue();
      if (zv.kind !== "text") throw new Error("unreachable: resolver requires a text zone");
      if (vv.kind !== "timestamp" && vv.kind !== "timestamptz") {
        throw new Error("unreachable: resolver requires a timestamp/timestamptz value");
      }
      m.charge(COSTS.timezone);
      m.guard();
      const micros = vv.micros;
      // ±infinity passes through unchanged (PG): no zone offset applies, zone not validated.
      if (micros === POS_INFINITY || micros === NEG_INFINITY) {
        return e.toTimestamptz ? timestamptzValue(micros) : timestampValue(micros);
      }
      const zr = resolveZone(zv.text);
      if (zr === undefined) {
        throw engineError("invalid_parameter_value", `time zone "${zv.text}" not recognized`);
      }
      return e.toTimestamptz
        ? timestamptzValue(localToInstantMicros(zr, micros))
        : timestampValue(instantToLocalMicros(zr, micros));
    }
    case "dateTrunc": {
      m.charge(COSTS.operatorEval);
      const uv = evalExpr(e.unit, row, env, m);
      const vv = evalExpr(e.value, row, env, m);
      const zv = e.zone !== null ? evalExpr(e.zone, row, env, m) : null;
      if (uv.kind === "null" || vv.kind === "null" || (zv !== null && zv.kind === "null")) {
        return nullValue();
      }
      if (uv.kind !== "text") throw new Error("unreachable: resolver requires a text unit");
      const unitS = uv.text;
      if (vv.kind === "timestamp") return timestampValue(dateTruncMicros(unitS, vv.micros));
      if (vv.kind === "interval") return intervalValue(dateTruncInterval(unitS, vv.iv));
      if (vv.kind === "timestamptz") {
        const mc = vv.micros;
        if (mc === POS_INFINITY || mc === NEG_INFINITY) {
          dateTruncMicros(unitS, mc); // still validate the unit
          return timestamptzValue(mc);
        }
        let zr: ZoneRef;
        if (zv !== null) {
          if (zv.kind !== "text") throw new Error("unreachable: resolver requires a text zone");
          const r = resolveZone(zv.text);
          if (r === undefined) {
            throw engineError("invalid_parameter_value", `time zone "${zv.text}" not recognized`);
          }
          zr = r;
        } else {
          zr = env.exec.session.timeZone;
        }
        m.charge(COSTS.timezone);
        m.guard();
        const local = instantToLocalMicros(zr, mc);
        const trunc = dateTruncMicros(unitS, local);
        return timestamptzValue(localToInstantMicros(zr, trunc));
      }
      throw new Error("unreachable: resolver restricts date_trunc to ts/tstz/interval");
    }
    case "extract": {
      m.charge(COSTS.operatorEval);
      const vv = evalExpr(e.value, row, env, m);
      if (vv.kind === "null") return nullValue();
      let src: ExtractSrc;
      if (vv.kind === "timestamp") src = { kind: "ts", micros: vv.micros };
      else if (vv.kind === "date") src = { kind: "date", days: vv.days };
      else if (vv.kind === "interval") src = { kind: "interval", iv: vv.iv };
      else if (vv.kind === "timestamptz") {
        const mc = vv.micros;
        // `epoch` is zone-independent (the instant); every other field decomposes in the session zone
        // — so only the zone-consulting fields charge `timezone`.
        if (e.field === "epoch" || mc === POS_INFINITY || mc === NEG_INFINITY) {
          src = { kind: "tstz", instant: mc, local: mc, offsetSecs: 0n };
        } else {
          const zr = env.exec.session.timeZone;
          m.charge(COSTS.timezone);
          m.guard();
          const local = instantToLocalMicros(zr, mc);
          const secs = mc >= 0n ? mc / 1_000_000n : -((-mc + 999_999n) / 1_000_000n);
          const off = BigInt(offsetAtRef(zr, secs).utoff);
          src = { kind: "tstz", instant: mc, local, offsetSecs: off };
        }
      } else {
        throw new Error("unreachable: resolver restricts EXTRACT to ts/tstz/date/interval");
      }
      return decimalValue(extractField(e.field, src));
    }
    case "dateConvert": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.inner, row, env, m);
      if (v.kind === "null") return nullValue();
      return evalDateConvert(v, e.to, env, m);
    }
    case "dateClock": {
      // A clock-relative date literal ('today'/'now'/'tomorrow'/'yesterday' — date.md §6): the
      // statement clock's day in the session zone + offsetDays. STABLE — the clock is read once
      // per statement, so every evaluation in the statement yields the same day.
      m.charge(COSTS.operatorEval);
      return dateClockValue(env.exec, env.rng, env.seam, m, e.offsetDays);
    }
    case "case": {
      // CASE is the ONE deliberate exception to "no short-circuit" (cost.md §3): conditions are
      // evaluated in order and evaluation STOPS at the first TRUE — a FALSE or NULL/UNKNOWN
      // condition falls through, and later arms (and their results) are NOT evaluated. Required
      // for PG semantics (e.g. `CASE WHEN a=0 THEN 0 ELSE 1/a END` must not divide by zero).
      // Charge the node, then only the conditions up to the match plus the selected result.
      m.charge(COSTS.operatorEval);
      for (const arm of e.arms) {
        const cv = evalExpr(arm.cond, row, env, m);
        if (cv.kind === "bool" && cv.value) {
          return coerceCaseValue(evalExpr(arm.result, row, env, m), e.coerceDecimal);
        }
      }
      return coerceCaseValue(evalExpr(e.els, row, env, m), e.coerceDecimal);
    }
    case "coalesce": {
      // COALESCE shares CASE's sanctioned short-circuit (cost.md §3): charge the node, then
      // evaluate arguments left to right — each at most ONCE — stopping at the first non-NULL,
      // which is the result. All-NULL → NULL. Later arguments are never evaluated, so an error
      // (or cost) in an unreached argument does not surface (grammar.md §51).
      m.charge(COSTS.operatorEval);
      for (const a of e.args) {
        const v = evalExpr(a, row, env, m);
        if (v.kind !== "null") {
          return coerceCaseValue(v, e.coerceDecimal);
        }
      }
      return nullValue();
    }
    case "greatestLeast": {
      // GREATEST/LEAST is EAGER (grammar.md §52): charge the node, then evaluate EVERY argument
      // (all must be, to be compared — GREATEST(1, 1/0) traps). NULL arguments are ignored; the
      // running winner is the max (greatest) or min (least) under the unified type's total order
      // (valueCmp). All-NULL → NULL. Non-NULL values are coerced to the unified type (integer →
      // decimal) before comparison so the comparator sees a single type.
      m.charge(COSTS.operatorEval);
      let best: Value | null = null;
      for (const a of e.args) {
        const v = evalExpr(a, row, env, m);
        if (v.kind === "null") continue;
        const cv = coerceCaseValue(v, e.coerceDecimal);
        if (best === null) {
          best = cv;
          continue;
        }
        const c = valueCmp(cv, best);
        if ((e.greatest && c > 0) || (!e.greatest && c < 0)) {
          best = cv;
        }
      }
      return best ?? nullValue();
    }
    case "scalarFunc": {
      // One operator_eval per call (the uniform weight); arguments charge their own.
      m.charge(COSTS.operatorEval);
      // quote_nullable is the one NON-STRICT scalar function: a NULL argument yields the text
      // 'NULL', not a propagated NULL, so it runs before the strict short-circuit loop below
      // (string-functions.md §3).
      if (e.func === "quote_nullable") {
        const v = evalExpr(e.args[0]!, row, env, m);
        return textValue(
          v.kind === "null" ? "NULL" : quoteLiteralText((v as { text: string }).text),
        );
      }
      const vals: Value[] = [];
      for (const a of e.args) {
        const v = evalExpr(a, row, env, m);
        if (v.kind === "null") return nullValue(); // NULL propagates
        vals.push(v);
      }
      if (e.func === "make_interval") {
        // make_interval — six integer components plus the f64 secs. years/months → months
        // field (×12), weeks/days → days field (×7), hours/mins/secs → micros; an i32/i64 field
        // overflow traps 22008 (functions.md §11). The one float step (secs → micros) is
        // correctly-rounded + deterministic, so the interval is in-contract. A f32 secs reads
        // as its exact f64 value (.value holds the binary64 of either width).
        const geti = (k: number): bigint => (vals[k] as { int: bigint }).int;
        const secMicros = f64ToMicros((vals[6] as { value: number }).value);
        return intervalValue(
          makeInterval(geti(0), geti(1), geti(2), geti(3), geti(4), geti(5), secMicros),
        );
      }
      if (e.func === "make_timestamp" || e.func === "make_timestamptz") {
        // make_timestamp / make_timestamptz — the make_interval siblings (functions.md §11).
        // Assemble the wall clock from the five integer fields + the f64 sec (an out-of-range field
        // traps 22008). make_timestamptz then interprets that wall clock in a zone (the session zone
        // for the 6-arg form, the trailing timezone text for the 7-arg form), charging one timezone
        // unit like AT TIME ZONE; an unrecognized explicit zone is 22023. A f32 sec reads as its
        // exact f64 value (.value holds the binary64 of either width).
        const geti = (k: number): bigint => (vals[k] as { int: bigint }).int;
        const wall = makeTimestamp(
          geti(0),
          geti(1),
          geti(2),
          geti(3),
          geti(4),
          (vals[5] as { value: number }).value,
        );
        if (e.func === "make_timestamp") return timestampValue(wall);
        // make_timestamptz: interpret the wall clock in a zone → a UTC instant.
        m.charge(COSTS.timezone);
        m.guard();
        if (vals.length === 7) {
          const zoneStr = (vals[6] as { text: string }).text;
          const z = resolveZone(zoneStr);
          if (z === undefined) {
            throw engineError("invalid_parameter_value", `time zone "${zoneStr}" not recognized`);
          }
          return timestamptzValue(localToInstantMicros(z, wall));
        }
        return timestamptzValue(localToInstantMicros(env.exec.session.timeZone, wall));
      }
      if (e.func === "make_date") {
        // make_date(year, month, day) — the make_timestamp sibling (functions.md §11): a negative
        // year is BC; year zero / a bad field / an out-of-range day count traps 22008.
        const geti = (k: number): bigint => (vals[k] as { int: bigint }).int;
        return dateValue(makeDate(geti(0), geti(1), geti(2)));
      }
      if (e.func === "current_date") {
        // CURRENT_DATE (functions.md §12, date.md §6): the statement clock's day in the session
        // zone — the 'today' literal as a function. STABLE; dateClockValue charges the timezone
        // unit.
        return dateClockValue(env.exec, env.rng, env.seam, m, 0n);
      }
      if (e.func === "date_part") {
        // date_part(field, source) — the float8-returning EXTRACT twin (timezones.md §9.2): the
        // shared extract kernel, then decimal → f64. The field is a RUNTIME text value
        // (case-insensitive, validated here — 22023 unrecognized / 0A000 unsupported-for-type,
        // like date_trunc's unit). A date source WIDENS TO MIDNIGHT and the timestamp matrix
        // applies (PG defines date_part(text, date) over ::timestamp — so 'hour' is 0 where
        // EXTRACT over a date is 0A000); the widen traps 22008 past the timestamp range. A
        // timestamptz source decomposes in the session zone with EXTRACT's exact selective
        // timezone charge. NULL propagation is the blanket case above.
        const field = (vals[0] as { text: string }).text.toLowerCase();
        const sv = vals[1]!;
        let src: ExtractSrc;
        if (sv.kind === "date") {
          src = { kind: "ts", micros: dateMidnightMicros(sv.days) };
        } else if (sv.kind === "timestamp") {
          src = { kind: "ts", micros: sv.micros };
        } else if (sv.kind === "interval") {
          src = { kind: "interval", iv: sv.iv };
        } else if (sv.kind === "timestamptz") {
          const mc = sv.micros;
          if (field === "epoch" || mc === POS_INFINITY || mc === NEG_INFINITY) {
            src = { kind: "tstz", instant: mc, local: mc, offsetSecs: 0n };
          } else {
            const zr = env.exec.session.timeZone;
            m.charge(COSTS.timezone);
            m.guard();
            const local = instantToLocalMicros(zr, mc);
            const secs = mc >= 0n ? mc / 1_000_000n : -((-mc + 999_999n) / 1_000_000n);
            const off = BigInt(offsetAtRef(zr, secs).utoff);
            src = { kind: "tstz", instant: mc, local, offsetSecs: off };
          }
        } else {
          throw new Error("unreachable: resolver restricts date_part to date/ts/tstz/interval");
        }
        const d = extractField(field, src);
        const f = Number(d.render());
        if (!Number.isFinite(f)) throw overflow("f64");
        return float64Value(f);
      }
      // uuid extractors (spec/design/functions.md §12): pure bit inspection; NULL for a non-RFC
      // variant (and, for the timestamp, any version other than 1/7). The NULL-input case is
      // already handled above.
      if (e.func === "uuid_extract_version") {
        const ver = uuidExtractVersion((vals[0] as { bytes: Uint8Array }).bytes);
        return ver === null ? nullValue() : intValue(ver);
      }
      if (e.func === "uuid_extract_timestamp") {
        const mc = uuidExtractTimestampMicros((vals[0] as { bytes: Uint8Array }).bytes);
        return mc === null ? nullValue() : timestamptzValue(mc);
      }
      // uuid generators (spec/design/entropy.md §3): draw from the per-statement seam (env.rng),
      // advancing the PRNG/counter. The NULL-arg case is handled above.
      if (e.func === "uuidv4") {
        return uuidValue(env.rng.uuidV4(env.seam));
      }
      if (e.func === "uuidv7") {
        const clock = env.rng.statementClockMicros(env.seam);
        // The optional interval arg shifts the embedded instant via the existing calendar-aware
        // timestamptz arithmetic (entropy.md §4).
        const shifted =
          vals.length === 1 ? tsShift(clock, (vals[0] as { iv: Interval }).iv, false) : clock;
        return uuidValue(env.rng.uuidV7(env.seam, shifted));
      }
      // current-time functions (spec/design/entropy.md §5): now() reads the statement clock ONCE and
      // reuses it (STABLE); clock_timestamp() reads the seam on every call (VOLATILE). Both return
      // the seam's micros directly as timestamptz.
      if (e.func === "now") {
        return timestamptzValue(env.rng.statementClockMicros(env.seam));
      }
      if (e.func === "clock_timestamp") {
        return timestamptzValue(env.rng.clockNowMicros(env.seam));
      }
      // Sequence value functions (spec/design/sequences.md §4/§6). nextval charges an additional
      // sequence_advance unit (the catalog-tuple read+rewrite) and mutates the per-statement pending
      // state; currval is a pure session-state read. The NULL-arg case is handled above (propagates).
      if (e.func === "nextval") {
        m.charge(COSTS.sequenceAdvance);
        return intValue(env.exec.seqNextval((vals[0] as { text: string }).text));
      }
      if (e.func === "currval") {
        return intValue(env.exec.seqCurrval((vals[0] as { text: string }).text));
      }
      // setval charges sequence_advance (it rewrites the catalog tuple, like nextval). Arity 2 →
      // isCalled defaults true; arity 3 → the boolean third argument.
      if (e.func === "setval") {
        m.charge(COSTS.sequenceAdvance);
        const isCalled = vals.length > 2 ? (vals[2] as { value: boolean }).value : true;
        return intValue(
          env.exec.seqSetval(
            (vals[0] as { text: string }).text,
            (vals[1] as { int: bigint }).int,
            isCalled,
          ),
        );
      }
      if (e.func === "lastval") {
        return intValue(env.exec.seqLastval());
      }
      // current_setting (spec/design/session.md §6.1): read the named session variable from the
      // session's variable map. The blanket NULL propagation above already returned NULL for a NULL
      // name / missing_ok argument, so both are non-NULL here. An unset name is 42704 UNLESS the
      // two-arg overload's missing_ok is true (→ NULL).
      if (e.func === "current_setting") {
        const name = (vals[0] as { text: string }).text;
        const missingOk = vals.length > 1 && (vals[1] as { value: boolean }).value;
        const got = env.exec.session.vars.get(name.toLowerCase());
        if (got !== undefined) {
          return textValue(got);
        }
        if (missingOk) {
          return nullValue();
        }
        throw engineError("undefined_object", "unrecognized configuration parameter: " + name);
      }
      // json/jsonb processing functions (B1, json-sql-functions.md §2). A jsonb arg is the node
      // directly; a json arg is parsed from its verbatim text on demand (json.md §4), then dispatched
      // to the same kernel. The NULL-input case was already handled by the blanket propagation above.
      if (e.func === "jsonb_typeof" || e.func === "json_typeof") {
        return textValue(jsonTypeofName(jsonArgNode(vals[0]!)));
      }
      if (e.func === "jsonb_array_length" || e.func === "json_array_length") {
        return intValue(jsonArrayLength(jsonArgNode(vals[0]!)));
      }
      if (e.func === "jsonb_strip_nulls") {
        return jsonbValue(jsonStripNulls(jsonArgNode(vals[0]!)));
      }
      if (e.func === "json_strip_nulls") {
        // json_strip_nulls returns json — render the stripped tree COMPACTLY (PG's json output
        // style), preserving the on-demand parse's key order.
        return jsonValue(jsonCompactOut(jsonStripNulls(jsonArgNode(vals[0]!))));
      }
      if (e.func === "jsonb_pretty") {
        return textValue(jsonPretty(jsonArgNode(vals[0]!)));
      }
      if (e.func === "to_jsonb") {
        // to_jsonb(anyelement) → the JSON image of the value (json-sql-functions.md §2). STRICT:
        // the NULL-input case is handled by the blanket propagation above.
        return jsonbValue(valueToNode(vals[0]!));
      }
      if (e.func === "to_json") {
        // to_json(anyelement) → the value's `json` image: a jsonb input renders canonical-spaced, a
        // json input verbatim, everything else the compact to_jsonb render (PG's datum_to_json) —
        // the same per-type rule the json builders embed. STRICT (NULL handled above).
        return jsonValue(elemJsonText(vals[0]!));
      }
      if (e.func === "json_scalar") {
        // JSON_SCALAR(v) → the value's JSON scalar as `json` (number/boolean/string), rendered
        // compact (json-sql-functions.md §5). STRICT (NULL handled above). The datetime/uuid/bytea/
        // interval/float sources are a deferred 0A000 follow-on.
        const v0 = vals[0]!;
        let node: JsonNode;
        switch (v0.kind) {
          case "int":
            // JSON_SCALAR of an integer wraps the bigint via the Decimal-from-i64 path (to_jsonb's Int).
            node = { kind: "number", dec: Decimal.fromBigInt(v0.int) };
            break;
          case "decimal":
            node = { kind: "number", dec: v0.dec };
            break;
          case "bool":
            node = { kind: "bool", value: v0.value };
            break;
          case "text":
            node = { kind: "string", value: v0.text };
            break;
          default:
            throw engineError(
              "feature_not_supported",
              "JSON_SCALAR of this type is not supported yet",
            );
        }
        return jsonValue(jsonCompactOut(node));
      }
      if (e.func === "json_serialize") {
        // JSON_SERIALIZE(v) → the value's text serialization: a json value verbatim, a jsonb value its
        // canonical (jsonbOut) render (json-sql-functions.md §5). STRICT (NULL handled above).
        const v0 = vals[0]!;
        if (v0.kind === "json") return textValue(v0.text);
        if (v0.kind === "jsonb") return textValue(jsonbOut(v0.node));
        throw new Error("BUG: resolver restricts JSON_SERIALIZE to json/jsonb");
      }
      if (e.func === "length") {
        // length(text) → i32 — the number of characters (Unicode code points). TS strings are
        // UTF-16, so [...s] (the code-point iterator) counts code points, NOT s.length, which
        // would over-count astral characters as surrogate pairs (string-functions.md §2/§3).
        const s = (vals[0] as { text: string }).text;
        return intValue(BigInt([...s].length));
      }
      if (e.func === "octet_length") {
        // octet_length(text) → i32 — the UTF-8 byte count, distinct from length's code-point
        // count (string-functions.md §3). utf8ByteLength encodes via TextEncoder.
        return intValue(BigInt(utf8ByteLength((vals[0] as { text: string }).text)));
      }
      if (e.func === "bit_length") {
        // bit_length(text) → i32 — the UTF-8 bit count = byte count × 8.
        return intValue(BigInt(utf8ByteLength((vals[0] as { text: string }).text) * 8));
      }
      if (e.func === "substr") {
        // substr(text, start[, count]) → text — the function form of SUBSTRING.
        const s = (vals[0] as { text: string }).text;
        const start = (vals[1] as { int: bigint }).int;
        const count = vals.length > 2 ? (vals[2] as { int: bigint }).int : null;
        return textValue(substrChars(s, start, count));
      }
      if (e.func === "left") {
        // left(text, n) → text — the first n characters (negative n drops the last |n|).
        const s = (vals[0] as { text: string }).text;
        return textValue(leftChars(s, (vals[1] as { int: bigint }).int));
      }
      if (e.func === "right") {
        // right(text, n) → text — the last n characters (negative n drops the first |n|).
        const s = (vals[0] as { text: string }).text;
        return textValue(rightChars(s, (vals[1] as { int: bigint }).int));
      }
      if (e.func === "lpad") {
        // lpad(text, length[, fill]) → text — pad/truncate on the LEFT (default fill a space).
        const s = (vals[0] as { text: string }).text;
        const length = (vals[1] as { int: bigint }).int;
        const fill = vals.length > 2 ? (vals[2] as { text: string }).text : " ";
        return textValue(padChars(s, length, fill, true));
      }
      if (e.func === "rpad") {
        // rpad(text, length[, fill]) → text — pad/truncate on the RIGHT (default fill a space).
        const s = (vals[0] as { text: string }).text;
        const length = (vals[1] as { int: bigint }).int;
        const fill = vals.length > 2 ? (vals[2] as { text: string }).text : " ";
        return textValue(padChars(s, length, fill, false));
      }
      if (e.func === "btrim") {
        // btrim(text[, chars]) → text — trim `chars`-set characters from both ends.
        const s = (vals[0] as { text: string }).text;
        const set = vals.length > 1 ? (vals[1] as { text: string }).text : " ";
        return textValue(trimChars(s, set, true, true));
      }
      if (e.func === "ltrim") {
        // ltrim(text[, chars]) → text — trim `chars`-set characters from the LEFT end.
        const s = (vals[0] as { text: string }).text;
        const set = vals.length > 1 ? (vals[1] as { text: string }).text : " ";
        return textValue(trimChars(s, set, true, false));
      }
      if (e.func === "rtrim") {
        // rtrim(text[, chars]) → text — trim `chars`-set characters from the RIGHT end.
        const s = (vals[0] as { text: string }).text;
        const set = vals.length > 1 ? (vals[1] as { text: string }).text : " ";
        return textValue(trimChars(s, set, false, true));
      }
      if (e.func === "replace") {
        // replace(text, from, to) → text — substring replace-all; empty `from` is a no-op
        // (String.replaceAll would otherwise splice `to` between every character — §3).
        const s = (vals[0] as { text: string }).text;
        const from = (vals[1] as { text: string }).text;
        const to = (vals[2] as { text: string }).text;
        return textValue(from === "" ? s : s.replaceAll(from, to));
      }
      if (e.func === "translate") {
        // translate(text, from, to) → text — per-character map/delete.
        const s = (vals[0] as { text: string }).text;
        const from = (vals[1] as { text: string }).text;
        const to = (vals[2] as { text: string }).text;
        return textValue(translateChars(s, from, to));
      }
      if (e.func === "repeat") {
        // repeat(text, n) → text — concatenate the string n times.
        const s = (vals[0] as { text: string }).text;
        return textValue(repeatText(s, (vals[1] as { int: bigint }).int));
      }
      if (e.func === "reverse") {
        // reverse(text) → text — the code points in reverse order. [...s] splits by code point
        // (not UTF-16 unit), so an astral character stays intact (string-functions.md §2).
        const s = (vals[0] as { text: string }).text;
        return textValue([...s].reverse().join(""));
      }
      if (e.func === "strpos") {
        // strpos(text, substring) → i32 — 1-based code-point position, else 0. indexOf gives a
        // UTF-16-unit offset; the match begins at a code-point boundary, so the code-point
        // position is the code-point count of the prefix + 1 (empty sub → 1, string-functions.md §3).
        const s = (vals[0] as { text: string }).text;
        const sub = (vals[1] as { text: string }).text;
        const idx = s.indexOf(sub);
        return intValue(idx < 0 ? 0n : BigInt([...s.slice(0, idx)].length + 1));
      }
      if (e.func === "split_part") {
        // split_part(text, delimiter, n) → text — the n-th split field.
        const s = (vals[0] as { text: string }).text;
        const delim = (vals[1] as { text: string }).text;
        return textValue(splitPart(s, delim, (vals[2] as { int: bigint }).int));
      }
      if (e.func === "starts_with") {
        // starts_with(text, prefix) → boolean — string begins with prefix.
        const s = (vals[0] as { text: string }).text;
        const pfx = (vals[1] as { text: string }).text;
        return boolValue(s.startsWith(pfx));
      }
      if (e.func === "ascii") {
        // ascii(text) → i32 — the code point of the first character (empty → 0). codePointAt(0)
        // returns the full code point (an astral char is one value, not a surrogate, §2).
        const s = (vals[0] as { text: string }).text;
        return intValue(BigInt(s.length === 0 ? 0 : (s.codePointAt(0) ?? 0)));
      }
      if (e.func === "chr") {
        // chr(int) → text — the one-character string for a code point.
        return textValue(chrText((vals[0] as { int: bigint }).int));
      }
      if (e.func === "initcap") {
        // initcap(text) → text — titlecase each word.
        return textValue(initcapAscii((vals[0] as { text: string }).text));
      }
      if (e.func === "to_hex") {
        // to_hex(int) → text — lowercase hex of the 64-bit two's-complement pattern. asUintN(64)
        // maps a negative i64 to its unsigned bit pattern, matching Rust/Go.
        const n = (vals[0] as { int: bigint }).int;
        return textValue(BigInt.asUintN(64, n).toString(16));
      }
      if (e.func === "encode") {
        // encode(bytea, format) → text — hex / base64 / escape.
        const bytes = (vals[0] as { bytes: Uint8Array }).bytes;
        const fmt = (vals[1] as { text: string }).text;
        return textValue(encodeBytea(bytes, fmt));
      }
      if (e.func === "decode") {
        // decode(text, format) → bytea — parse hex / base64 / escape back to bytes.
        const s = (vals[0] as { text: string }).text;
        const fmt = (vals[1] as { text: string }).text;
        return byteaValue(decodeText(s, fmt));
      }
      if (e.func === "quote_literal") {
        // quote_literal(text) → text — wrap as a SQL string literal.
        return textValue(quoteLiteralText((vals[0] as { text: string }).text));
      }
      if (e.func === "quote_ident") {
        // quote_ident(text) → text — wrap as a SQL identifier.
        return textValue(quoteIdentText((vals[0] as { text: string }).text));
      }
      if (e.func === "pi") {
        // pi() — the constant π, no operand (float.md §8). In-contract: Math.PI is the same f64
        // literal in every core.
        return float64Value(Math.PI);
      }
      if (e.func === "div") {
        // div(a, b): the truncated integer quotient at scale 0, computed EXACTLY as (a − a%b)/b —
        // a − a%b is exactly q·b, so the division is exact and roundToScale(0) only drops the
        // (already-zero) fraction. rem() traps 22012 on a zero divisor. Integer operands promote.
        const toDec = (v: Value): Decimal =>
          v.kind === "int" ? Decimal.fromBigInt(v.int) : (v as { dec: Decimal }).dec;
        const a = toDec(vals[0]!);
        const b = toDec(vals[1]!);
        const q = a.sub(a.rem(b)).div(b);
        return decimalValue(q.roundToScale(0));
      }
      if (e.func === "gcd") {
        // gcd: integer operands → Euclid over bigint (the result must fit the promoted type, else
        // 22003 — gcd(MinInt64, 0) and the rare i16-cap edge); a decimal operand → exact decimal
        // Euclid at scale max(sₐ, s_b). gcd(0, 0) = 0.
        const a0 = vals[0]!;
        const b0 = vals[1]!;
        if (a0.kind === "int" && b0.kind === "int") {
          const g = gcdBigint(a0.int, b0.int);
          if (!inRange(e.result, g)) throw overflow(e.result);
          return intValue(g);
        }
        const toDec = (v: Value): Decimal =>
          v.kind === "int" ? Decimal.fromBigInt(v.int) : (v as { dec: Decimal }).dec;
        return decimalValue(gcdDecimalValue(toDec(a0), toDec(b0)));
      }
      if (e.func === "lcm") {
        // lcm: |a/gcd·b|. Integer → the promoted type (an out-of-range magnitude → 22003); a
        // decimal operand → exact at scale max(sₐ, s_b). lcm(_, 0) = 0.
        const a0 = vals[0]!;
        const b0 = vals[1]!;
        if (a0.kind === "int" && b0.kind === "int") {
          const l = lcmBigint(a0.int, b0.int);
          if (!inRange(e.result, l)) throw overflow(e.result);
          return intValue(l);
        }
        const toDec = (v: Value): Decimal =>
          v.kind === "int" ? Decimal.fromBigInt(v.int) : (v as { dec: Decimal }).dec;
        return decimalValue(lcmDecimalValue(toDec(a0), toDec(b0)));
      }
      if (e.func === "factorial") {
        // factorial(n) = n! at scale 0. A negative operand → 22003. Each multiply is metered
        // (size-scaled decimal_work, guarded) so the cost ceiling bounds a large factorial before
        // its limb work runs (cost.md §3, §13); a product over the value cap traps 22003.
        const n = (vals[0] as { int: bigint }).int;
        if (n < 0n)
          throw engineError(
            "numeric_value_out_of_range",
            "factorial of a negative number is undefined",
          );
        let acc = Decimal.fromBigInt(1n);
        for (let k = 2n; k <= n; k++) {
          const kd = Decimal.fromBigInt(k);
          m.charge(COSTS.decimalWork * BigInt(workMul(acc, kd) - 1));
          m.guard();
          acc = acc.mul(kd);
        }
        return decimalValue(acc);
      }
      if (e.func === "width_bucket") {
        // width_bucket(op, low, high, count): the histogram bucket index. count > 0 (else 2201G);
        // dispatch numeric vs float on the operand; the raw index is range-checked to int4.
        const count = (vals[3] as { int: bigint }).int;
        if (count <= 0n) throw widthBucketErr("count must be greater than zero");
        const op = vals[0]!;
        // The resolver guarantees the value trio is homogeneous: all float → the float kernel;
        // otherwise the numeric kernel (integers promote to decimal).
        const fv = (v: Value): number => (v as { value: number }).value;
        const toDec = (v: Value): Decimal =>
          v.kind === "int" ? Decimal.fromBigInt(v.int) : (v as { dec: Decimal }).dec;
        const idx =
          op.kind === "f32" || op.kind === "f64"
            ? widthBucketFloat(fv(op), fv(vals[1]!), fv(vals[2]!), count)
            : widthBucketNumeric(toDec(op), toDec(vals[1]!), toDec(vals[2]!), count);
        if (!inRange("i32", idx)) throw overflow("i32");
        return intValue(idx);
      }
      if (e.func === "scale") {
        // scale(numeric) → the display (fractional-digit) scale, as i32 (always ≤ 16383).
        return intValue(BigInt((vals[0] as { dec: Decimal }).dec.scale));
      }
      if (e.func === "min_scale") {
        // min_scale(numeric) → the smallest exact scale (trailing fractional zeros dropped).
        return intValue(BigInt(minScaleOf((vals[0] as { dec: Decimal }).dec)));
      }
      if (e.func === "trim_scale") {
        // trim_scale(numeric) → the value re-scaled down to its min_scale (exact; the dropped
        // digits are zeros, so roundToScale does not round).
        const d = (vals[0] as { dec: Decimal }).dec;
        return decimalValue(d.roundToScale(minScaleOf(d)));
      }
      const v0 = vals[0];
      // Float scalar functions (float.md §8): dispatch on the operand being a float value. Per the
      // catalog, only abs is operand-typed (result "promoted"); every other float func returns
      // f64 (result "f64") — so the result Value's width is e.result, not argWidth. The
      // computation is done in binary64; abs frounds for a f32 result via e.result.
      if (v0.kind === "f32" || v0.kind === "f64") {
        if (e.func === "pow") {
          // pow(x, y): both operands are float (promoted to one width at resolve); result f64.
          const v1 = vals[1] as { value: number };
          return evalFloatPow(v0.value, v1.value, e.result);
        }
        if (e.func === "atan2") {
          // atan2(y, x): y is vals[0], x is vals[1] (both widened to f64). Quadrant-aware; no trap.
          const v1 = vals[1] as { value: number };
          return float64Value(Math.atan2(v0.value, v1.value));
        }
        // round(x, n): n is an int operand; the unary funcs ignore it.
        const places = vals.length > 1 ? Number((vals[1] as { int: bigint }).int) : 0;
        return evalFloatFunc(e.func, v0.value, places, e.result);
      }
      if (e.func === "abs") {
        if (v0.kind === "int") {
          // abs over an integer: |x| then range-check at the result type's boundary
          // (abs(i16 -32768) → 22003), exactly like neg.
          let n = v0.int;
          if (n < 0n) n = -n;
          if (!inRange(e.result, n)) throw overflow(e.result);
          return intValue(n);
        }
        return decimalValue((v0 as { dec: Decimal }).dec.abs());
      }
      if (e.func === "sign") {
        // sign over a decimal → numeric at scale 0 (-1 / 0 / +1).
        const d = (v0 as { dec: Decimal }).dec;
        const s = d.limbs.length === 0 ? 0n : d.neg ? -1n : 1n;
        return decimalValue(Decimal.fromBigInt(s));
      }
      if (e.func === "ceil" || e.func === "floor" || e.func === "trunc") {
        // ceil/ceiling/floor/trunc over decimal (and integer, promoted) — the EXACT-numeric
        // overloads (decimal.md §6, functions.md §9). The float overloads returned above. ceil/
        // floor round to scale 0 toward ±∞ (a round-up carry can trap 22003); trunc truncates
        // toward zero to scale 0 or its n-place argument (never overflows).
        const dd = v0.kind === "int" ? Decimal.fromBigInt(v0.int) : (v0 as { dec: Decimal }).dec;
        if (e.func === "ceil") return decimalValue(dd.ceil());
        if (e.func === "floor") return decimalValue(dd.floor());
        const places = vals.length > 1 ? Number((vals[1] as { int: bigint }).int) : 0;
        return decimalValue(dd.truncPlaces(places));
      }
      if (
        e.func === "sqrt" ||
        e.func === "exp" ||
        e.func === "ln" ||
        e.func === "log10" ||
        e.func === "log" ||
        e.func === "pow"
      ) {
        // EXACT-numeric transcendentals over decimal (decimal.md §8). Float operands returned
        // above; here the operand is decimal — a PG-faithful arbitrary-precision kernel,
        // byte-identical across cores. Domain errors: sqrt of a negative and the power domain
        // errors → 2201F; ln/log of a non-positive → 2201E; exp/power overflow → 22003.
        const a = (v0 as { dec: Decimal }).dec;
        switch (e.func) {
          case "sqrt":
            return decimalValue(a.decSqrt());
          case "exp":
            return decimalValue(a.decExp());
          case "ln":
            return decimalValue(a.decLn());
          case "log10":
            return decimalValue(a.decLog10());
          case "log": {
            const num = vals.length > 1 ? (vals[1] as { dec: Decimal }).dec : null;
            return decimalValue(num ? Decimal.decLog(a, num) : a.decLog10());
          }
          default: {
            // pow (the catalog `power(decimal,decimal)`, renamed to "pow" at resolve)
            const exp = (vals[1] as { dec: Decimal }).dec;
            return decimalValue(Decimal.decPower(a, exp));
          }
        }
      }
      // round
      const d = v0.kind === "int" ? Decimal.fromBigInt(v0.int) : (v0 as { dec: Decimal }).dec;
      const places = vals.length > 1 ? Number((vals[1] as { int: bigint }).int) : 0;
      return decimalValue(d.roundPlaces(places));
    }
    case "arrayFunc": {
      // A polymorphic array function (spec/design/array-functions.md §3). One operator_eval per call;
      // arguments charge their own. NULL handling is per-kernel (the introspectors propagate, the
      // builders are non-strict), so — unlike "scalarFunc" — there is no blanket NULL short-circuit.
      m.charge(COSTS.operatorEval);
      const vals: Value[] = [];
      for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      return evalArrayFunc(e.func, vals);
    }
    case "rangeFunc": {
      // A polymorphic range accessor (spec/design/range-functions.md §1, RF1). One operator_eval per
      // call; arguments charge their own. STRICT (a NULL range → NULL), handled in the kernel.
      m.charge(COSTS.operatorEval);
      const vals: Value[] = [];
      for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      return evalRangeFunc(e.func, vals);
    }
    case "rangeCtor": {
      // A range CONSTRUCTOR call (spec/design/range-functions.md §2, RF2). One operator_eval (like
      // the range accessors); arguments charge their own evaluation. Non-strict — the kernel turns a
      // NULL bound into an infinite bound, so there is no blanket NULL short-circuit.
      m.charge(COSTS.operatorEval);
      const vals: Value[] = [];
      for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      return evalRangeCtor(e.elem, vals);
    }
    case "rangeOp": {
      // A range BOOLEAN operator (spec/design/range-functions.md §3, RF3). One operator_eval; the
      // operands charge their own evaluation. STRICT — a NULL operand short-circuits to NULL in
      // evalRangeOp.
      m.charge(COSTS.operatorEval);
      const l = evalExpr(e.args[0]!, row, env, m);
      const r = evalExpr(e.args[1]!, row, env, m);
      return evalRangeOp(e.op, l, r, e.elem);
    }
    case "rangeSetOp": {
      // A range SET operator (spec/design/range-functions.md §4). One operator_eval; the operands
      // charge their own evaluation. STRICT — a NULL operand short-circuits to NULL in evalRangeSetOp.
      m.charge(COSTS.operatorEval);
      const l = evalExpr(e.args[0]!, row, env, m);
      const r = evalExpr(e.args[1]!, row, env, m);
      return evalRangeSetOp(e.op, l, r);
    }
    case "variadic": {
      // A VARIADIC argument-counting call (spec/design/array-functions.md §12). One operator_eval
      // (the per-element/arg count walk is unmetered, like the array introspectors §3.3); arguments
      // charge their own. Non-strict — no blanket NULL short-circuit. The two forms differ: the
      // spread form counts the args' null-ness (never NULL); the VARIADIC-array form returns NULL on
      // a NULL whole-array, else counts the array's flattened elements' null-ness.
      m.charge(COSTS.operatorEval);
      const wantNulls = e.func === "num_nulls";
      if (e.arrayForm) {
        const v = evalExpr(e.args[0]!, row, env, m);
        if (v.kind === "null") return nullValue();
        if (v.kind !== "array")
          throw new Error("resolver restricts a VARIADIC operand to an array");
        return intValue(BigInt(countNulls(v.elements, wantNulls)));
      }
      const vals: Value[] = [];
      for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      return intValue(BigInt(countNulls(vals, wantNulls)));
    }
    case "jsonBuild": {
      // A VARIADIC json/jsonb builder (json-sql-functions.md §2). Gather the argument values (the
      // spread form directly; the VARIADIC-array form spreads the lone array — a NULL whole-array →
      // SQL NULL), then build an array / object node. Non-strict — a NULL argument is JSON null
      // (array) or a value (object), so no blanket NULL short-circuit.
      m.charge(COSTS.operatorEval);
      let vals: Value[];
      if (e.arrayForm) {
        const v = evalExpr(e.args[0]!, row, env, m);
        if (v.kind === "null") return nullValue();
        if (v.kind !== "array")
          throw new Error("resolver restricts a VARIADIC operand to an array");
        vals = v.elements;
      } else {
        vals = [];
        for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      }
      m.charge(COSTS.operatorEval * BigInt(vals.length));
      m.guard();
      if (e.buildKind === "array") {
        if (e.json) {
          // json_build_array → a `json` value: each element's own json text image (a json arg
          // verbatim, a jsonb arg spaced, else compact), joined `, ` inside `[...]`.
          return jsonValue(`[${vals.map(elemJsonText).join(", ")}]`);
        }
        // jsonb_build_array → a jsonb value: each argument's valueToNode image, canonical render.
        return jsonbValue({ kind: "array", elements: vals.map(valueToNode) });
      }
      // object
      if (vals.length % 2 !== 0) {
        throw engineError(
          "invalid_parameter_value",
          "argument list must have even number of elements",
        );
      }
      if (e.json) {
        // json_build_object → a `json` value keeping argument order + duplicate keys: the key as a
        // JSON string, `" : "` (space-colon-space) before the value's element json text, members
        // joined `, ` inside `{...}`.
        const parts: string[] = [];
        for (let i = 0; i < vals.length; i += 2) {
          const key = objectKeyText(vals[i]!, i + 1);
          parts.push(
            `${jsonCompactOut({ kind: "string", value: key })} : ${elemJsonText(vals[i + 1]!)}`,
          );
        }
        return jsonValue(`{${parts.join(", ")}}`);
      }
      // jsonb_build_object → a jsonb object: (key, valueToNode) members, last-wins dedup + canonical
      // key sort via makeObject.
      const members: JsonMember[] = [];
      for (let i = 0; i < vals.length; i += 2) {
        const key = objectKeyText(vals[i]!, i + 1);
        members.push({ key, value: valueToNode(vals[i + 1]!) });
      }
      return jsonbValue(jsonMakeObject(members));
    }
    case "jsonSetInsert": {
      // jsonb_set / jsonb_insert (json-sql-functions.md §2): STRICT path mutation. Any NULL argument
      // (or a NULL path element) → SQL NULL.
      m.charge(COSTS.operatorEval);
      const target = evalExpr(e.args[0]!, row, env, m);
      const pathV = evalExpr(e.args[1]!, row, env, m);
      const valueV = evalExpr(e.args[2]!, row, env, m);
      const flagV = evalExpr(e.args[3]!, row, env, m);
      m.guard();
      if (
        target.kind === "null" ||
        pathV.kind === "null" ||
        valueV.kind === "null" ||
        flagV.kind === "null"
      ) {
        return nullValue();
      }
      // Extract the text[] path (a NULL element propagates a SQL NULL result, like the `#-` path).
      if (pathV.kind !== "array") throw new Error("resolver guarantees a text[] path");
      const path: string[] = [];
      for (const el of pathV.elements) {
        if (el.kind !== "text") return nullValue(); // a NULL path element propagates
        path.push(el.text);
      }
      const node = jsonArgNode(target);
      const valueNode = jsonArgNode(valueV);
      const flag = flagV.kind === "bool" && flagV.value;
      const out =
        e.mode === "set"
          ? jsonSetPathKernel(node, path, valueNode, flag)
          : jsonInsertPathKernel(node, path, valueNode, flag);
      return jsonbValue(out);
    }
    case "jsonObjectFromArrays": {
      // json_object / jsonb_object (json-sql-functions.md §2): build an object from text array(s).
      m.charge(COSTS.operatorEval);
      // STRICT: a NULL whole-array argument → SQL NULL.
      const arrays: (string | null)[][] = [];
      for (const a of e.args) {
        const v = evalExpr(a, row, env, m);
        if (v.kind === "null") return nullValue();
        arrays.push(valueToOptTextArray(v));
      }
      // Pair up keys/values: one array of alternating k/v (even length), or two equal-length arrays.
      const pairs: [string | null, string | null][] = [];
      if (arrays.length === 1) {
        const flat = arrays[0]!;
        if (flat.length % 2 !== 0) {
          throw engineError("array_subscript_error", "array must have even number of elements");
        }
        for (let i = 0; i < flat.length; i += 2) pairs.push([flat[i]!, flat[i + 1]!]);
      } else {
        if (arrays[0]!.length !== arrays[1]!.length) {
          throw engineError("array_subscript_error", "mismatched array dimensions");
        }
        for (let i = 0; i < arrays[0]!.length; i++) pairs.push([arrays[0]![i]!, arrays[1]![i]!]);
      }
      m.charge(COSTS.operatorEval * BigInt(pairs.length));
      m.guard();
      // A NULL key → 22004; a NULL value → JSON null, else a JSON string of its text.
      if (e.json) {
        const parts: string[] = [];
        for (const [k, v] of pairs) {
          if (k === null) throw objectKeyNull();
          const val = v === null ? "null" : jsonCompactOut({ kind: "string", value: v });
          parts.push(`${jsonCompactOut({ kind: "string", value: k })} : ${val}`);
        }
        return jsonValue(`{${parts.join(", ")}}`);
      }
      const members: JsonMember[] = [];
      for (const [k, v] of pairs) {
        if (k === null) throw objectKeyNull();
        members.push({
          key: k,
          value: v === null ? { kind: "null" } : { kind: "string", value: v },
        });
      }
      return jsonbValue(jsonMakeObject(members));
    }
    case "jsonPathFn": {
      // A scalar jsonpath query function (P2, jsonpath.md §5). STRICT: a NULL ctx/path → NULL.
      m.charge(COSTS.operatorEval);
      const ctx = evalExpr(e.args[0]!, row, env, m);
      const path = evalExpr(e.args[1]!, row, env, m);
      const seq = evalJsonpath(ctx, path);
      if (seq === null) return nullValue();
      // Charge per produced item so a runaway `[*]` fan-out stays cost-proportional.
      m.charge(COSTS.operatorEval * BigInt(seq.length));
      m.guard();
      switch (e.pathFnKind) {
        case "exists":
          return boolValue(seq.length > 0);
        case "queryFirst":
          return seq.length > 0 ? jsonbValue(seq[0]!) : nullValue();
        case "queryArray":
          return jsonbValue({ kind: "array", elements: seq });
        case "match": {
          // jsonb_path_match / @@: the path must produce EXACTLY one boolean item.
          if (seq.length === 1 && seq[0]!.kind === "bool") {
            return boolValue(seq[0]!.value);
          }
          throw engineError(
            "singleton_sql_json_item_required",
            "single boolean result is expected",
          );
        }
      }
      break;
    }
    case "jsonSqlFn": {
      // A SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY (json-sql-functions.md §5,
      // S2). A NULL context / path → NULL; a SQL/JSON (class-22) error honors ON ERROR.
      m.charge(COSTS.operatorEval);
      const cv = evalExpr(e.args[0]!, row, env, m);
      const pv = evalExpr(e.args[1]!, row, env, m);
      if (cv.kind === "null" || pv.kind === "null") return nullValue();
      let seq: JsonNode[] | null;
      try {
        seq = evalJsonpath(cv, pv);
      } catch (err) {
        // A SQL/JSON (data-exception) error is caught by ON ERROR; anything else (a cost abort,
        // etc.) propagates.
        if (isSqljsonError(err)) {
          return applyJsonBehavior(e.onError, err, e.returning, env, m);
        }
        throw err;
      }
      // A NULL ctx/path already returned above; evalJsonpath can still report no match as null.
      if (seq === null) return nullValue();
      m.charge(COSTS.operatorEval * BigInt(seq.length));
      m.guard();
      return evalJsonSqlResult(
        e.sqlKind,
        seq,
        e.returning,
        e.decimal,
        e.wrapper,
        e.onEmpty,
        e.onError,
        env,
        m,
      );
    }
    case "subquery": {
      // A correlated subquery (spec/design/grammar.md §26): re-executed once per outer row. Push
      // the current row onto the outer-row stack, run the inner plan, fold its accrued cost into
      // this meter, plus one operator_eval for the node.
      m.charge(COSTS.operatorEval);
      const r = env.runSubquery(e.plan, [...env.outer, row]);
      m.charge(r.cost);
      if (e.subKind === "scalar") {
        if (r.rows.length > 1) {
          throw engineError(
            "cardinality_violation",
            "more than one row returned by a subquery used as an expression",
          );
        }
        // 0 rows -> NULL (the static type was settled at resolve).
        return r.rows.length === 0 ? nullValue() : r.rows[0]![0]!;
      }
      if (e.subKind === "exists") {
        // EXISTS ignores the select list entirely and is never NULL.
        return { kind: "bool", value: r.rows.length > 0 !== e.negated };
      }
      if (e.subKind === "quantified") {
        // A correlated quantified subquery (array-functions.md §11.6): gather the body's single
        // column into an array and run the SAME 3VL fold as the array form.
        const lv = evalExpr(e.lhs!, row, env, m);
        const elements = r.rows.map((rr) => rr[0]!);
        return quantifiedMembership(e.op!, e.all!, lv, arrayValue(elements), m);
      }
      // in
      const lv = evalExpr(e.lhs!, row, env, m);
      const list = r.rows.map((rr) => rr[0]!);
      return inMembership(lv, list, e.negated, m);
    }
    case "inValues": {
      // A folded uncorrelated `IN (subquery)` — the list is constant; test membership per row.
      m.charge(COSTS.operatorEval);
      const lv = evalExpr(e.lhs, row, env, m);
      return inMembership(lv, e.list, e.negated, m);
    }
    case "quantified": {
      // A quantified array comparison `lhs op ANY/ALL(array)` (array-functions.md §11) — the array
      // spelling of IN, the 3VL fold over the array's flattened elements.
      m.charge(COSTS.operatorEval);
      const lv = evalExpr(e.lhs, row, env, m);
      const av = evalExpr(e.array, row, env, m);
      return quantifiedMembership(e.op, e.all, lv, av, m);
    }
  }
}

// quantifiedMembership is the three-valued membership fold for `lhs op ANY/ALL(array)`
// (array-functions.md §11), the generalization of inMembership to all five comparison operators and
// both quantifiers. A NULL array -> NULL; otherwise, over the flattened elements, ANY/SOME (all=false)
// is the OR-fold (TRUE if any `lhs op e` is TRUE, else NULL if any is NULL, else FALSE; empty ->
// FALSE) and ALL (all=true) the AND-fold (FALSE if any is FALSE, else NULL if any is NULL, else TRUE;
// empty -> TRUE). Each element comparison charges one operator_eval (+ size-scaled decimal_work),
// exactly like inMembership, so max_cost bounds the walk (54P01).
export function quantifiedMembership(
  op: BinaryOp,
  all: boolean,
  lv: Value,
  av: Value,
  m: Meter,
): Value {
  if (av.kind === "null") return nullValue();
  if (av.kind !== "array") throw new Error("BUG: the resolver requires an array right operand");
  let anyNull = false;
  for (const e of av.elements) {
    m.charge(COSTS.operatorEval);
    m.charge(COSTS.decimalWork * BigInt(decimalCmpWork(lv, e) - 1));
    m.guard();
    const t = quantifiedCmp3(op, lv, e);
    if (t === "true") {
      // ANY short-circuits TRUE; ALL keeps going (TRUE is its neutral element).
      if (!all) return { kind: "bool", value: true };
    } else if (t === "false") {
      // ALL short-circuits FALSE; ANY keeps going (FALSE is its neutral element).
      if (all) return { kind: "bool", value: false };
    } else {
      anyNull = true;
    }
  }
  // Drained without a short-circuit: a NULL seen -> UNKNOWN; else the quantifier's identity (ALL ->
  // TRUE, ANY -> FALSE — also the empty-array result).
  return anyNull ? nullValue() : { kind: "bool", value: all };
}

// quantifiedCmp3 is the per-element three-valued comparison `lhs op e` for a quantified node,
// normalizing a mixed-width float pair to f64 first (the resolver admits f32 vs f64,
// matching the compare node's promote — here the array elements are runtime values, so the widen
// happens per element). Bottoms out in the value module's eq3/lt3/gt3 kernels.
//
// A composite operand pair routes through the composite TOTAL ORDER (valueCmp), NOT the bare-ROW 3VL
// eq3/lt3/gt3 (array-functions.md §13): PostgreSQL's = ANY(addr[]) dispatches on the composite =
// operator = record_eq, which is DEFINITE with NULL fields comparable (ROW('a',NULL)::addr =
// ANY(ARRAY[ROW('a',NULL)::addr]) is TRUE), the same total order array_eq / @> already use for
// composite elements (array.md §5). A whole-element NULL is still UNKNOWN — the operator stays strict
// at the value level — so the resolver-guaranteed same-type pair is composite-vs-composite or
// composite-vs-NULL.
export function quantifiedCmp3(op: BinaryOp, x: Value, e: Value): ThreeValued {
  if (x.kind === "composite" || e.kind === "composite") {
    if (x.kind === "null" || e.kind === "null") return "unknown";
    const ord = valueCmp(x, e);
    let matched: boolean;
    switch (op) {
      case "eq":
        matched = ord === 0;
        break;
      case "ne":
        matched = ord !== 0;
        break;
      case "lt":
        matched = ord < 0;
        break;
      case "gt":
        matched = ord > 0;
        break;
      case "le":
        matched = ord <= 0;
        break;
      default: // ge
        matched = ord >= 0;
    }
    return matched ? "true" : "false";
  }
  if (x.kind === "f32" && e.kind === "f64") x = float64Value(x.value);
  else if (x.kind === "f64" && e.kind === "f32") e = float64Value(e.value);
  switch (op) {
    case "eq":
      return eq3(x, e);
    case "ne":
      return not3(eq3(x, e));
    case "lt":
      return lt3(x, e);
    case "gt":
      return gt3(x, e);
    case "le":
      return or3(lt3(x, e), eq3(x, e));
    default: // ge
      return or3(gt3(x, e), eq3(x, e));
  }
}

// likeMatch is the SQL LIKE matcher (spec/design/grammar.md §22): `%` matches any (possibly
// empty) run of characters, `_` exactly one character, and `\` (the default escape) makes the
// next pattern character literal. It iterates by Unicode CODE POINT via Array.from (NOT `str[i]`
// / charCodeAt, the UTF-16 trap) so astral characters match `_` — a CLAUDE.md §8 determinism
// surface. Two-pointer greedy backtracking, identical across cores. It throws a 22025 error when
// the escape character is the LAST pattern character reached during matching (PostgreSQL's "LIKE
// pattern must not end with escape character") — data-dependent, since an earlier mismatch
// returns false first.
export function likeMatch(subject: string, pattern: string): boolean {
  const s = Array.from(subject);
  const p = Array.from(pattern);
  let si = 0;
  let pi = 0;
  // The last '%' position in the pattern (a backtrack point) and the subject index when it was
  // taken; -1 until a '%' has been seen.
  let starPi = -1;
  let starSi = 0;
  while (si < s.length) {
    if (pi < p.length && p[pi] === "\\") {
      // Escape: the next pattern character must match the subject literally.
      if (pi + 1 >= p.length) {
        throw engineError(
          "invalid_escape_sequence",
          "LIKE pattern must not end with escape character",
        );
      }
      if (s[si] === p[pi + 1]) {
        si++;
        pi += 2;
        continue;
      }
      // literal mismatch → fall through to backtrack
    } else if (pi < p.length && p[pi] === "_") {
      si++;
      pi++;
      continue;
    } else if (pi < p.length && p[pi] === "%") {
      starPi = pi;
      starSi = si;
      pi++;
      continue;
    } else if (pi < p.length && p[pi] === s[si]) {
      si++;
      pi++;
      continue;
    }
    // Mismatch: backtrack to the last '%' (it absorbs one more subject character), else no.
    if (starPi >= 0) {
      pi = starPi + 1;
      starSi++;
      si = starSi;
      continue;
    }
    return false;
  }
  // Subject consumed: any pattern remainder must be all '%' to match.
  while (pi < p.length && p[pi] === "%") pi++;
  return pi === p.length;
}

// evalArith computes an integer op with exact bigint, throwing 22012 on a zero divisor
// and 22003 if the result falls outside the declared result type (the i16+i16 →
// i16 boundary — spec/design/functions.md §7). The MinInt64/-1 cases trap to match the
// Rust/Go checked-op behaviour (bigint would not overflow on its own).
export function evalArith(op: BinaryOp, x: bigint, y: bigint, result: ScalarType): Value {
  let v: bigint;
  switch (op) {
    case "add":
      v = x + y;
      break;
    case "sub":
      v = x - y;
      break;
    case "mul":
      v = x * y;
      break;
    case "div":
      if (y === 0n) throw engineError("division_by_zero", "division by zero");
      if (x === I64_MIN && y === -1n) throw overflow(result);
      v = x / y; // bigint truncates toward zero
      break;
    default: // "mod"
      if (y === 0n) throw engineError("division_by_zero", "division by zero");
      // `x % -1` is mathematically 0 for every x; bigint computes it as 0n exactly (no
      // overflow). Unlike division, modulo by -1 has no out-of-range result, so it does NOT
      // trap — matching PostgreSQL and the i16/i32 widths (spec/design/types.md §3).
      v = x % y; // bigint remainder takes the dividend's sign
      break;
  }
  if (!inRange(result, v)) throw overflow(result);
  return intValue(v);
}

// evalFloatArith computes one IEEE float operation (float.md §5). The trap model (float.md §3):
//   - x / 0 and x % 0 trap 22012 (division_by_zero) for EVERY numerator except NaN — Inf/0 and 0/0
//     trap, only NaN/0 propagates to NaN (matching PG);
//   - a FINITE op whose true result overflows the float range to ±Inf traps 22003 (e.g. 1e308*10);
//   - an Inf/NaN OPERAND otherwise propagates by IEEE (Inf+1=Inf, Inf-Inf=NaN, NaN*0=NaN) — no trap.
// For f32 every result is Math.fround'd (true binary32 rounding — the TS-specific discipline);
// the overflow check is then re-applied because fround can push a finite double past binary32 range.
// `%` is IEEE remainder via JS `%` (which is fmod — truncated, dividend's sign), exact, never
// overflows.
export function evalFloatArith(op: BinaryOp, x: number, y: number, result: ScalarType): Value {
  const f32 = result === "f32";
  const finiteInputs = Number.isFinite(x) && Number.isFinite(y);
  let r: number;
  switch (op) {
    case "add":
      r = x + y;
      break;
    case "sub":
      r = x - y;
      break;
    case "mul":
      r = x * y;
      break;
    case "div":
      // x / 0 traps for every numerator except NaN, which propagates (NaN/0 = NaN, matching PG).
      if (y === 0 && !Number.isNaN(x)) throw engineError("division_by_zero", "division by zero");
      r = x / y;
      break;
    default: // "mod"
      if (y === 0 && !Number.isNaN(x)) throw engineError("division_by_zero", "division by zero");
      r = x % y; // JS % is fmod: truncated, takes the dividend's sign; exact, finite for finite x,y
      break;
  }
  if (f32) r = Math.fround(r);
  // A finite-operand op that produced a non-finite result overflowed the (binary32 after fround, or
  // binary64) range → trap 22003. An Inf/NaN that came FROM an operand propagates and is NOT a trap.
  if (finiteInputs && !Number.isFinite(r)) throw overflow(result);
  return f32 ? float32Value(r) : float64Value(r);
}

// evalFloatFunc evaluates a unary float scalar function (float.md §8) over a float value `x`,
// producing a value of width `result` (always f64 here except abs, whose result is the operand
// width). `places` is the second argument of round(x, n) (ignored by the others). An Inf/NaN operand
// propagates through the exact functions; the transcendentals call native Math.* (exempted — the R
// tag absorbs cross-core ULP differences). Domain / overflow errors trap (float.md §8):
//   sqrt(neg) → 22003; ln(0)/ln(neg) → 22003; exp overflow → 22003; sin/cos/tan never trap.
// PG's exact RADIANS_PER_DEGREE literal (float.c) — shared by radians/degrees so the single IEEE
// multiply/divide is byte-identical cross-core and matches PG (in-contract).
// biome-ignore lint/correctness/noPrecisionLoss: PG's float.c RADIANS_PER_DEGREE literal, kept verbatim so the f64 rounding is byte-identical cross-core and matches PG; trimming digits would diverge.
export const RADIANS_PER_DEGREE = 0.0174532925199432957692;
export function evalFloatFunc(
  func: ScalarFuncName,
  x: number,
  places: number,
  result: ScalarType,
): Value {
  const out = (r: number): Value => {
    // result is f64 for all but abs; abs's result is the operand width, so fround for f32.
    if (result === "f32") {
      const f = Math.fround(r);
      // abs cannot overflow (|finite| stays finite at the same width); a NaN/Inf propagates.
      return float32Value(f);
    }
    return float64Value(r);
  };
  switch (func) {
    case "abs":
      return out(Math.abs(x)); // |NaN| = NaN, |±Inf| = +Inf — propagation, no trap
    case "ceil":
      return out(Math.ceil(x));
    case "floor":
      return out(Math.floor(x));
    case "trunc":
      return out(Math.trunc(x));
    case "round":
      return out(roundFloatHalfAway(x, places));
    case "sqrt":
      // sqrt(neg) is a DOMAIN error → 22003 (NaN stays input-only). sqrt(NaN)=NaN, sqrt(+Inf)=+Inf,
      // sqrt(-0)=-0 all propagate. IEEE mandates sqrt correctly-rounded, so it is in-contract.
      if (x < 0)
        throw engineError(
          "numeric_value_out_of_range",
          "cannot take square root of a negative number",
        );
      return out(Math.sqrt(x));
    case "exp": {
      // exp overflow (e.g. exp(710)) → 22003. A NaN/±Inf operand propagates (exp(+Inf)=+Inf,
      // exp(-Inf)=0, exp(NaN)=NaN). Transcendental — exempted (R tag).
      const r = Math.exp(x);
      if (Number.isFinite(x) && !Number.isFinite(r)) throw overflow(result);
      return out(r);
    }
    case "ln":
      // ln(0) → 22003; ln(neg) → 22003 (domain). ln(+Inf)=+Inf, ln(NaN)=NaN propagate.
      if (x === 0) throw engineError("numeric_value_out_of_range", "cannot take logarithm of zero");
      if (x < 0)
        throw engineError(
          "numeric_value_out_of_range",
          "cannot take logarithm of a negative number",
        );
      return out(Math.log(x));
    case "log10":
      if (x === 0) throw engineError("numeric_value_out_of_range", "cannot take logarithm of zero");
      if (x < 0)
        throw engineError(
          "numeric_value_out_of_range",
          "cannot take logarithm of a negative number",
        );
      return out(Math.log10(x));
    case "sin":
      return out(Math.sin(x));
    case "cos":
      return out(Math.cos(x));
    case "tan":
      return out(Math.tan(x));
    case "cbrt":
      // cbrt has no domain restriction: cbrt(-8) = -2, cbrt(±Inf) = ±Inf, cbrt(NaN) = NaN.
      return out(Math.cbrt(x));
    case "radians":
      // radians/degrees — a single correctly-rounded IEEE op (multiply/divide) by PG's exact
      // RADIANS_PER_DEGREE literal (float.c), so byte-identical cross-core (in-contract).
      return out(x * RADIANS_PER_DEGREE);
    case "degrees":
      return out(x / RADIANS_PER_DEGREE);
    case "asin":
      // asin domain is [-1, 1]: a finite |x| > 1 (and ±Inf, magnitude > 1) is out of range →
      // 22003, exactly PG; a NaN operand propagates (no trap).
      if (!Number.isNaN(x) && (x < -1 || x > 1))
        throw engineError("numeric_value_out_of_range", "input is out of range");
      return out(Math.asin(x));
    case "acos":
      // acos shares asin's domain [-1, 1]: |x| > 1 (or ±Inf) → 22003, NaN propagates.
      if (!Number.isNaN(x) && (x < -1 || x > 1))
        throw engineError("numeric_value_out_of_range", "input is out of range");
      return out(Math.acos(x));
    case "atan":
      // atan is defined on all of ℝ (no domain trap); atan(±Inf) = ±π/2, atan(NaN) = NaN.
      return out(Math.atan(x));
    case "cot":
      // cot(x) = 1/tan(x) (no Math.cot; 1/tan bit-matches PG). cot(0) = +Inf (no trap).
      return out(1 / Math.tan(x));
    case "sinh":
      // sinh/cosh overflow to ±Inf with NO trap (PG-faithful, unlike exp/pow). NaN propagates.
      return out(Math.sinh(x));
    case "cosh":
      return out(Math.cosh(x));
    case "tanh":
      return out(Math.tanh(x));
    case "asinh":
      return out(Math.asinh(x));
    case "acosh":
      // acosh domain [1, ∞): a finite x < 1 → 22003 (a NaN propagates, acosh(+Inf) = +Inf).
      if (!Number.isNaN(x) && x < 1)
        throw engineError("numeric_value_out_of_range", "input is out of range");
      return out(Math.acosh(x));
    case "atanh":
      // atanh domain [-1, 1]: a finite |x| > 1 (and ±Inf) → 22003; atanh(±1) = ±Inf is admissible.
      if (!Number.isNaN(x) && (x < -1 || x > 1))
        throw engineError("numeric_value_out_of_range", "input is out of range");
      return out(Math.atanh(x));
    case "sign":
      // sign over a float (EXACT, in-contract): sign(NaN) = sign(±0) = 0, sign(±Inf) = ±1 (PG
      // dsign tests x > 0 / x < 0, so a NaN falls through to 0).
      return out(x > 0 ? 1 : x < 0 ? -1 : 0);
    default:
      throw typeError("internal: unsupported float scalar function " + func);
  }
}

// evalFloatPow evaluates pow(x, y) → f64 (float.md §8): native Math.pow (transcendental,
// exempted), trapping 22003 on a finite-input overflow to ±Inf (e.g. pow(10, 400)); a NaN/±Inf
// operand propagates per IEEE. result is f64 (the catalog), so no fround.
export function evalFloatPow(x: number, y: number, result: ScalarType): Value {
  const r = x ** y;
  if (Number.isFinite(x) && Number.isFinite(y) && !Number.isFinite(r)) throw overflow(result);
  return result === "f32" ? float32Value(Math.fround(r)) : float64Value(r);
}

// roundFloatHalfAway rounds a float to `places` decimal places, HALF AWAY FROM ZERO (jed's one
// mode — float.md §8). For an Inf/NaN it returns the value unchanged (propagation). It scales by
// 10^places, rounds half-away (negatives by magnitude — Math.round is half-UP, wrong for ties), then
// unscales. Done in binary64; the caller frounds for a f32 result of round (catalog result is
// f64, so in practice no fround). Note: this is approximate at the binary level (the scale
// factor is not exactly representable) — acceptable since float rounding is in the R-tag surface.
export function roundFloatHalfAway(x: number, places: number): number {
  if (!Number.isFinite(x)) return x;
  const f = 10 ** places;
  const scaled = x * f;
  const r = scaled < 0 ? -Math.round(-scaled) : Math.round(scaled);
  return r / f;
}

// evalCast evaluates a (non-NULL) CAST to target. int→int range-checks (22003); int→decimal
// widens then coerces to the typmod; decimal→int rounds half-away to scale 0 then range-checks
// (22003); decimal→decimal re-scales to the typmod (spec/design/decimal.md §6).
export function evalCast(v: Value, target: ScalarType, typmod: DecimalTypmod | null): Value {
  // The JSON cast matrix (spec/design/json.md §6.1). text → json/jsonb is the only runtime text
  // cast (every other text cast target is resolver-rejected): json validates + stores verbatim
  // (22P02 on malformed); jsonb parses + canonicalizes.
  if (v.kind === "text") {
    // text → text: the identity (a varchar(n) length, if any, truncates at the cast eval site —
    // types.md §15). The resolver only produces a text→text cast node when a length is present.
    if (isText(target)) return v;
    if (isJson(target)) {
      validateJson(v.text);
      return jsonValue(v.text);
    }
    if (isJsonb(target)) return jsonbValue(jsonbIn(v.text));
    // text → uuid (the uuid cast slice, casts.toml/types.md §14): the PG-flexible uuid_in parser; a
    // malformed string traps 22P02.
    if (isUuid(target)) return uuidValue(decodeUuidLiteral(v.text));
    // text → numeric/boolean (the runtime-text-cast slice, grammar.md §36): the same per-row
    // coercion the `type 'string'` literal folds at resolve, run here over the runtime string. The
    // resolver admits only int/decimal/float/bool targets for a text source (uuid/json/jsonb are the
    // arms above). Malformed → 22P02, out of range → 22003 (per row).
    if (isBool(target)) return boolValue(parseBoolLiteral(v.text));
    if (isDecimal(target)) return decimalValue(coerceDecimal(parseDecimalLiteral(v.text), typmod));
    if (isFloat(target)) {
      const n = parseFloatLiteral(v.text, target);
      return target === "f32" ? float32Value(n) : float64Value(n);
    }
    // An int target (i16/i32/i64): parseIntLiteral range-checks against target (22003).
    return intValue(parseIntLiteral(v.text, target));
  }
  // uuid → text (canonical lowercase 8-4-4-4-12) and uuid → bytea (the 16 raw bytes) — the uuid
  // cast slice (casts.toml/types.md §14).
  if (v.kind === "uuid") {
    if (isText(target)) return textValue(renderUuid(v.bytes));
    if (isBytea(target)) return byteaValue(v.bytes);
    throw new Error("BUG: resolver rejects this uuid cast target");
  }
  // bytea → uuid (the uuid cast slice — a jed cast PG lacks): exactly 16 raw bytes; any other length
  // traps 22P02 (the wrong-width body — no PG code to match).
  if (v.kind === "bytea") {
    if (isUuid(target)) {
      if (v.bytes.length !== 16) {
        throw engineError(
          "invalid_text_representation",
          `invalid length for type uuid: ${v.bytes.length} bytes (expected 16)`,
        );
      }
      return uuidValue(v.bytes);
    }
    throw new Error("BUG: resolver rejects this bytea cast target");
  }
  // json → text is the identity on the verbatim bytes; json → jsonb re-parses + canonicalizes;
  // json → json is the identity.
  if (v.kind === "json") {
    if (isText(target)) return textValue(v.text);
    if (isJson(target)) return jsonValue(v.text);
    if (isJsonb(target)) return jsonbValue(jsonbIn(v.text));
    throw new Error("BUG: resolver rejects this json cast target");
  }
  // jsonb → text / json renders the canonical form (jsonb_out); jsonb → jsonb is the identity.
  if (v.kind === "jsonb") {
    if (isText(target)) return textValue(jsonbOut(v.node));
    if (isJson(target)) return jsonValue(jsonbOut(v.node));
    if (isJsonb(target)) return v;
    throw new Error("BUG: resolver rejects this jsonb cast target");
  }
  if (v.kind === "bool") {
    // boolean → boolean is the identity cast (`x::boolean` on a boolean). boolean → i32 (the
    // boolean cast slice, casts.toml): true → 1, false → 0. The resolver guarantees the only
    // non-bool target is i32.
    if (isBool(target)) return v;
    return intValue(v.value ? 1n : 0n);
  }
  if (v.kind === "int") {
    // i32 → boolean (the boolean cast slice, casts.toml): 0 → false, any nonzero (incl. negative)
    // → true. The resolver guarantees the source is i32, so v.int is already in i32 range.
    if (isBool(target)) return boolValue(v.int !== 0n);
    if (isDecimal(target)) return decimalValue(coerceDecimal(Decimal.fromBigInt(v.int), typmod));
    // int → float (explicit, lossy): nearest binary representable, then fround for f32. Exact
    // for |int| ≤ 2^53; a larger i64 may round. Never traps (float.md §6).
    if (isFloat(target)) return makeFloat(target, Number(v.int));
    if (!inRange(target, v.int)) throw overflow(target);
    return intValue(v.int);
  }
  if (v.kind === "decimal") {
    if (isDecimal(target)) return decimalValue(coerceDecimal(v.dec, typmod));
    // decimal → float (explicit, lossy): nearest binary to the exact decimal (Number of the
    // canonical decimal string is the IEEE conversion). A huge decimal → ±Inf traps 22003 rather
    // than yielding Infinity (the finite-overflow rule, float.md §6).
    if (isFloat(target)) {
      const d = Number(v.dec.render());
      if (!Number.isFinite(d)) throw overflow(target);
      return makeFloat(target, d);
    }
    const n = v.dec.toBigIntRound();
    if (n === null || !inRange(target, n)) throw overflow(target);
    return intValue(n);
  }
  if (v.kind === "f32" || v.kind === "f64") {
    // float → float (the tower): f32 → f64 lossless (widen); f64 → f32 frounds
    // (lossy), trapping 22003 if a finite double rounds beyond binary32 range. float→float never
    // converts a NaN/±Inf to an error — those are first-class values that propagate (float.md §6).
    if (isFloat(target)) return makeFloatCast(target, v.value);
    // float → int (explicit): round HALF AWAY FROM ZERO to an integer, range-check (22003). NaN/
    // ±Inf → 22003 (NaN stays input-only — a float never becomes a NaN integer; float.md §6). A
    // documented PG divergence (PG rounds half-to-even; jed keeps one engine-wide mode).
    if (isInteger(target)) {
      if (!Number.isFinite(v.value)) throw overflow(target);
      const n = floatToIntHalfAway(v.value);
      if (!inRange(target, n)) throw overflow(target);
      return intValue(n);
    }
    // float → decimal (explicit): the EXACT decimal of the binary value (float.md §6 — the unique
    // exact value of the IEEE float, NOT Number#toString's shortest round-trip, which would diverge
    // cross-core), then the typmod's scale coercion. NaN/±Inf → 22003 (decimal is finite).
    if (isDecimal(target)) {
      if (!Number.isFinite(v.value)) throw overflow(target);
      const exact =
        v.kind === "f32" ? Decimal.exactFromFloat32(v.value) : Decimal.exactFromFloat64(v.value);
      return decimalValue(coerceDecimal(exact, typmod));
    }
    throw typeError("internal: unsupported float cast target");
  }
  throw typeError("internal: non-numeric cast operand");
}

// makeFloat builds a float Value at `ty`, trapping 22003 if a finite-source value rounds to ±Inf
// (the finite-overflow rule; the source here is already finite — only f32 rounding can push a
// finite double beyond binary32 range). Used by int/decimal → float.
export function makeFloat(ty: ScalarType, n: number): Value {
  const r = ty === "f32" ? Math.fround(n) : n;
  if (!Number.isFinite(r)) throw overflow(ty);
  return ty === "f32" ? float32Value(r) : float64Value(r);
}

// makeFloatCast builds a float Value at `ty` from a float SOURCE value, where a NaN/±Inf source is
// preserved (it propagates — float→float is not a finite operation). Only a FINITE double that
// frounds past binary32 range traps 22003. Used by float → float casts.
export function makeFloatCast(ty: ScalarType, n: number): Value {
  if (ty === "f64") return float64Value(n);
  const r = Math.fround(n);
  // A finite double beyond binary32 range frounds to ±Inf → trap; a NaN/±Inf source stays as-is.
  if (Number.isFinite(n) && !Number.isFinite(r)) throw overflow(ty);
  return float32Value(r);
}

// floatToIntHalfAway rounds a finite float to a bigint, HALF AWAY FROM ZERO (jed's one rounding
// mode — decimal.md §3; float.md §6). Math.round rounds half UP (toward +Inf), which differs for
// negative ties (Math.round(-2.5) = -2, want -3), so negatives are handled by magnitude. BigInt of
// a non-integer JS number throws, so the rounded (integral) double is converted.
export function floatToIntHalfAway(v: number): bigint {
  const r = v < 0 ? -Math.round(-v) : Math.round(v);
  return BigInt(r);
}

// toDecimal widens a numeric value to Decimal (an integer operand of decimal arithmetic).
export function toDecimal(v: Value): Decimal {
  if (v.kind === "decimal") return v.dec;
  if (v.kind === "int") return Decimal.fromBigInt(v.int);
  throw typeError("internal: non-numeric decimal operand");
}

// decimalArithWork is the decimal_work W of an arithmetic node — which group-count formula
// applies per op (spec/design/cost.md §3 "decimal_work"). The evaluator charges W − 1 before
// the op runs.
export function decimalArithWork(op: BinaryOp, a: Decimal, b: Decimal): number {
  switch (op) {
    case "add":
    case "sub":
      return workLinear(a, b);
    case "mul":
      return workMul(a, b);
    case "div":
      return workDiv(a, b);
    default: // "mod"
      return workMod(a, b);
  }
}

// decimalCmpWork is the decimal_work W of a comparison over a decimal(-promotable) pair — the
// aligned linear formula after int→decimal promotion; 1 (no charge) for any other pair,
// including a NULL side, where no decimal compare runs (spec/design/cost.md §3 "decimal_work").
export function decimalCmpWork(a: Value, b: Value): number {
  if (a.kind === "decimal" && b.kind === "decimal") return workLinear(a.dec, b.dec);
  if (a.kind === "decimal" && b.kind === "int") return workLinear(a.dec, Decimal.fromBigInt(b.int));
  if (a.kind === "int" && b.kind === "decimal") return workLinear(Decimal.fromBigInt(a.int), b.dec);
  return 1;
}

// varlenCompareWork is the varlen_compare W of a comparison over a variable-length scalar pair —
// the SHORTER operand's length (code points for text, bytes for bytea), clamped to >= 1. A byte /
// code-point comparison stops at the first differing position or the end of the shorter operand,
// so min is a true upper bound on the work (spec/design/cost.md §3 "varlen_compare"). Any other
// pair — including a NULL side or a non-varlen type — returns 1 (no charge). [...s] counts code
// points (NOT s.length — the UTF-16 trap, CLAUDE.md §8).
export function varlenCompareWork(a: Value, b: Value): number {
  let n: number;
  if (a.kind === "text" && b.kind === "text") {
    n = Math.min([...a.text].length, [...b.text].length);
  } else if (a.kind === "bytea" && b.kind === "bytea") {
    n = Math.min(a.bytes.length, b.bytes.length);
  } else {
    return 1;
  }
  return n < 1 ? 1 : n;
}

// opCostOverrides maps an operator NAME to its per-operator cost base, for the OPERATORS rows whose
// catalog cost is non-default (functions.md §8). Empty while every built-in uses the uniform
// operatorEval; authoring a cost in catalog.toml populates it (a pure data change, no code). The
// cost === 0 sentinel means "use operatorEval". Built once at module load from the generated table.
export const opCostOverrides: Map<string, bigint> = new Map(
  OPERATORS.filter((o) => o.cost !== 0).map((o) => [o.name, BigInt(o.cost)]),
);

// operatorCost is the cost an operator's evaluation charges: its catalog cost base if authored, else
// the uniform operatorEval (cost.md §3). The size===0 fast path keeps the common all-default case a
// single check, so no per-node map lookup happens until a weight is actually tuned. The arithmetic
// and comparison op strings ARE the catalog names ("add", "lt", …); neg/not/and/or pass a literal.
export function operatorCost(name: string): bigint {
  if (opCostOverrides.size === 0) return COSTS.operatorEval;
  return opCostOverrides.get(name) ?? COSTS.operatorEval;
}

// evalDecimalArith evaluates decimal arithmetic with PG's result-scale rules
// (spec/design/decimal.md §4), throwing 22003 at the cap and 22012 on a zero divisor/modulus.
export function evalDecimalArith(op: BinaryOp, a: Decimal, b: Decimal): Decimal {
  switch (op) {
    case "add":
      return a.add(b);
    case "sub":
      return a.sub(b);
    case "mul":
      return a.mul(b);
    case "div":
      return a.div(b);
    default: // "mod"
      return a.rem(b);
  }
}
