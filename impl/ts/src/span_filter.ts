import { runSync, type Steps } from "./execution.ts";
// Encoded-span predicates for the columnar lanes (spec/design/packed-leaf.md §11 Track A4).
//
// A columnar lane (aggColumnar / projectColumnar) evaluates its WHERE per stored record while it walks
// the tree. compileSpanPred turns the predicate into a tree that mirrors evalFilter exactly — each AND
// node keeps its guard + `and` charge on entry and its left-then-right short-circuit — whose leaves are
// either a span atom, evaluated on the leaf page's stored bytes without building a value, or a generic
// predicate evaluated by the ordinary evaluator over a reusable scratch row. An atom charges exactly
// what the evaluator charges for its subtree, and whenever its column's bytes are not available in
// place (a Decoded leaf, an out-of-line or compressed value, a body that is not exactly well-formed) it
// evaluates through the ordinary evaluator instead — so results, charges, and errors are the
// evaluator's by construction. Mirrors impl/rust/src/executor/span_filter.rs.

import type { ColType } from "./catalog.ts";
import { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import {
  evalExpr,
  evalExprSteps,
  evalFilterSteps,
  operatorCost,
  synchronousEvalEnv,
} from "./eval.ts";
import { collectTouched, type EvalEnv, type RExpr } from "./executor.ts";
import { encodeValue, fixedValueWidth, resolveUnfetchedSelfSteps } from "./format.ts";
import { colAt, type PNode, type Span } from "./pmap.ts";
import type { Row } from "./storage.ts";
import { inRange, isBytea, isInteger, isText, widthBytes } from "./types.ts";
import { isTrue, nullValue, type Value } from "./value.ts";

const UTF8 = new TextEncoder();

// ScratchRow is the row a lane feeds the ordinary evaluator: column c holds record i's resolved value
// once ensure filled it for the current record; a predicate is evaluated only after every column it
// reads was filled. A stamp per column makes a fill happen at most once per record however many
// predicates read the column.
export class ScratchRow {
  readonly vals: Row;
  private readonly stamp: number[];
  private generation = 1;
  // The record generation whose every touched column a plain filter filled (the columns may still
  // hold deferred values — the evaluator resolves those on touch).
  private allFilled = 0;

  constructor(width: number) {
    this.vals = new Array(width).fill(nullValue());
    this.stamp = new Array(width).fill(0);
  }

  // nextRecord starts a new record: every column's fill becomes stale.
  nextRecord(): void {
    this.generation++;
  }

  // fillAll reads every column in cols straight from the leaf (deferred values stay deferred — the
  // evaluator resolves those on touch), marking the record fully filled.
  fillAll(n: PNode, i: number, cols: number[]): void {
    for (const c of cols) this.vals[c] = colAt(n, i, c);
    this.allFilled = this.generation;
  }

  // ensure resolves column c of record i into the scratch row, once per record — the inline decode,
  // chain read, or decompression the row path's resolveColumns performs.
  ensure(n: PNode, i: number, c: number): void {
    return runSync(this.ensureSteps(n, i, c));
  }

  *ensureSteps(n: PNode, i: number, c: number): Steps<void> {
    if (this.stamp[c] === this.generation) return;
    const v = colAt(n, i, c);
    this.vals[c] = v.kind === "unfetched" ? yield* resolveUnfetchedSelfSteps(v.ref) : v;
    this.stamp[c] = this.generation;
  }

  // read returns record i's resolved column c: the value a predicate already resolved for this record,
  // else read straight from the leaf (not stored). Callers use it only for a variable-width column read
  // beside a predicate — where reusing the predicate's decode pays — and readLeafColumn otherwise.
  read(n: PNode, i: number, c: number): Value {
    return runSync(this.readSteps(n, i, c));
  }

  *readSteps(n: PNode, i: number, c: number): Steps<Value> {
    if (this.allFilled === this.generation) {
      const v = this.vals[c]!;
      return v.kind === "unfetched" ? yield* resolveUnfetchedSelfSteps(v.ref) : v;
    }
    if (this.stamp[c] === this.generation) return this.vals[c]!;
    return yield* readLeafColumnSteps(n, i, c);
  }
}

// readLeafColumn returns record i's column c read straight from the leaf, a deferred value resolved.
export function* readLeafColumnSteps(n: PNode, i: number, c: number): Steps<Value> {
  const v = colAt(n, i, c);
  return v.kind === "unfetched" ? yield* resolveUnfetchedSelfSteps(v.ref) : v;
}

// laneReuse reports, per column, whether a lane should reuse the value its filter resolved (a
// variable-width column beside a filter) rather than read the leaf again (a fixed-width slot decodes
// cheaper than the reuse costs).
export function laneReuse(colTypes: ColType[], filtered: boolean): boolean[] {
  return colTypes.map((ty) => filtered && fixedValueWidth(ty) === null);
}

// LaneFilter is a lane's compiled WHERE: the span evaluator when the predicate has at least one span
// atom, else the original lane filter — every touched column read into the scratch row (a deferred
// value resolves when the evaluator touches it), then evalFilter — which is cheaper for a predicate
// the span compiler cannot help with.
export type LaneFilter =
  | { kind: "span"; pred: SpanPred }
  | { kind: "plain"; expr: RExpr; cols: number[] };

export function compileLaneFilter(
  filter: RExpr,
  colTypes: ColType[],
  mask: boolean[],
  env: EvalEnv,
): LaneFilter {
  const pred = compileSpanPred(filter, colTypes, mask, env);
  if (hasAtom(pred)) return { kind: "span", pred };
  const cols: number[] = [];
  for (let c = 0; c < mask.length; c++) if (mask[c]) cols.push(c);
  return { kind: "plain", expr: filter, cols };
}

// laneKeep reports whether record i of n passes, starting a new scratch record.
export function* laneKeepSteps(
  f: LaneFilter,
  n: PNode,
  i: number,
  scratch: ScratchRow,
  env: EvalEnv,
  m: Meter,
): Steps<boolean> {
  scratch.nextRecord();
  if (f.kind === "span") return yield* spanEvalFilterSteps(f.pred, n, i, scratch, env, m);
  scratch.fillAll(n, i, f.cols);
  return yield* evalFilterSteps(f.expr, scratch.vals, env, m);
}

function hasAtom(p: SpanPred): boolean {
  if (p.kind === "and") return hasAtom(p.lhs) || hasAtom(p.rhs);
  return p.kind === "atom";
}

// SpanPred is a compiled lane predicate (the file comment). A generic predicate carries the touched
// columns it reads (its collectTouched set — the same analysis that built the lane's mask).
export type SpanPred =
  | { kind: "and"; lhs: SpanPred; rhs: SpanPred }
  | { kind: "atom"; atom: SpanAtom }
  | { kind: "generic"; expr: RExpr; cols: number[] };

// The array operators an atom evaluates: col @> k (or k <@ col), col <@ k (or k @> col), col && k.
type ArrayOpKind = "colContainsK" | "kContainsCol" | "overlaps";

// An array element body's delimiting — the element types whose value equality is byte equality of
// their stored bodies (strict element equality over text/bytea is byte order; integers are a
// fixed-width bijective codec). width === 0 is a u16-length-prefixed body.
type ElemCodec = { width: number; text: boolean };

// SpanAtom is a predicate leaf evaluated on its column's stored bytes. expr is the original leaf,
// evaluated by the ordinary evaluator when the bytes are not available in place. base is the operator
// charge plus the constant operand's replayed charge.
type SpanAtom = { expr: RExpr; col: number } & (
  | { kind: "nullTest"; negated: boolean }
  | {
      kind: "bytesEq";
      negated: boolean;
      text: boolean;
      k: Uint8Array;
      kWidth: number;
      base: bigint;
    }
  | {
      kind: "arrayOp";
      op: ArrayOpKind;
      elem: ElemCodec;
      kElems: Uint8Array[];
      kHasNull: boolean;
      base: bigint;
      spans: number[];
    }
);

// The reusable span scratch (single-threaded).
const SPAN: Span = { start: 0, end: 0 };

// compileSpanPred compiles filter for a lane over a single base relation whose column types are
// colTypes and whose touched set is mask. Never fails: a leaf the span evaluator does not recognize
// stays a generic predicate.
export function compileSpanPred(
  filter: RExpr,
  colTypes: ColType[],
  mask: boolean[],
  env: EvalEnv,
): SpanPred {
  if (filter.kind === "and") {
    return {
      kind: "and",
      lhs: compileSpanPred(filter.lhs, colTypes, mask, env),
      rhs: compileSpanPred(filter.rhs, colTypes, mask, env),
    };
  }
  const atom = compileSpanAtom(filter, colTypes, mask, env);
  if (atom !== null) return { kind: "atom", atom };
  const reads: boolean[] = new Array(mask.length).fill(false);
  collectTouched(filter, 0, reads);
  const cols: number[] = [];
  for (let c = 0; c < mask.length; c++) if (reads[c] && mask[c]) cols.push(c);
  return { kind: "generic", expr: filter, cols };
}

// spanEvalFilter reports whether record i of n passes — evalFilter over the record's row, with
// identical charges.
export function* spanEvalFilterSteps(
  p: SpanPred,
  n: PNode,
  i: number,
  scratch: ScratchRow,
  env: EvalEnv,
  m: Meter,
): Steps<boolean> {
  switch (p.kind) {
    case "and":
      m.guard();
      m.charge(operatorCost("and"));
      return (
        (yield* spanEvalFilterSteps(p.lhs, n, i, scratch, env, m)) &&
        (yield* spanEvalFilterSteps(p.rhs, n, i, scratch, env, m))
      );
    case "atom": {
      const keep = evalSpan(p.atom, n, i, m);
      if (keep !== null) return keep;
      yield* scratch.ensureSteps(n, i, p.atom.col);
      return isTrue(yield* evalExprSteps(p.atom.expr, scratch.vals, env, m));
    }
    default:
      for (const c of p.cols) yield* scratch.ensureSteps(n, i, c);
      return isTrue(yield* evalExprSteps(p.expr, scratch.vals, env, m));
  }
}

// evalSpan evaluates an atom on the stored bytes, charging exactly what the evaluator charges for the
// leaf; null (charging nothing) when the bytes are not available in place, so the caller evaluates the
// leaf through the evaluator.
function evalSpan(a: SpanAtom, n: PNode, i: number, m: Meter): boolean | null {
  const p = n.packed;
  if (a.kind === "nullTest") {
    m.guard();
    m.charge(COSTS.operatorEval);
    const isNull = p === undefined ? n.vals[i]![a.col]!.kind === "null" : p.isNull(i, a.col);
    return isNull !== a.negated;
  }
  if (p === undefined) return null;
  if (!p.span(i, a.col, SPAN)) {
    // A NULL operand: the operator is NULL (not TRUE) and charges no length work.
    m.guard();
    m.charge(a.base);
    return false;
  }
  const buf = p.payload;
  // The inline-plain body follows a 0x00 presence tag; any other tag is out of line or compressed.
  if (SPAN.start >= SPAN.end || buf[SPAN.start] !== 0x00) return null;
  const start = SPAN.start + 1;
  const end = SPAN.end;
  if (a.kind === "bytesEq") {
    // A u16-length-prefixed body, exactly the prefix plus that many bytes.
    if (end - start < 2 || end - start - 2 !== ((buf[start]! << 8) | buf[start + 1]!)) return null;
    let width = end - start - 2;
    if (a.text) {
      width = utf8CodePoints(buf, start + 2, end);
      if (width < 0) return null;
    }
    m.guard();
    m.charge(a.base);
    const w = Math.max(Math.min(width, a.kWidth), 1);
    m.charge(COSTS.varlenCompare * BigInt(w - 1));
    m.guard();
    return bytesEqual(buf, start + 2, end, a.k) !== a.negated;
  }
  const colHasNull = arrayElementSpans(buf, start, end, a.elem, a.spans);
  if (colHasNull === null) return null;
  m.guard();
  m.charge(a.base);
  const spans = a.spans;
  const inCol = (kb: Uint8Array): boolean => {
    for (let s = 0; s < spans.length; s += 2) {
      if (bytesEqual(buf, spans[s]!, spans[s + 1]!, kb)) return true;
    }
    return false;
  };
  const inK = (s: number): boolean => {
    for (const kb of a.kElems) if (bytesEqual(buf, spans[s]!, spans[s + 1]!, kb)) return true;
    return false;
  };
  switch (a.op) {
    case "colContainsK":
      // Every k element (a NULL one matches nothing) equals some column element.
      return !a.kHasNull && a.kElems.every(inCol);
    case "kContainsCol":
      // Every column element (a NULL one matches nothing) equals some k element.
      if (colHasNull) return false;
      for (let s = 0; s < spans.length; s += 2) if (!inK(s)) return false;
      return true;
    default:
      for (let s = 0; s < spans.length; s += 2) if (inK(s)) return true;
      return false;
  }
}

function bytesEqual(buf: Uint8Array, start: number, end: number, k: Uint8Array): boolean {
  if (end - start !== k.length) return false;
  for (let j = 0; j < k.length; j++) if (buf[start + j] !== k[j]) return false;
  return true;
}

// utf8CodePoints counts the code points of buf[start, end) — or returns -1 when the bytes are not
// well-formed UTF-8 (overlong forms, surrogates, values past U+10FFFF, truncated sequences), exactly the
// bytes the decoder rejects.
export function utf8CodePoints(buf: Uint8Array, start: number, end: number): number {
  let n = 0;
  let i = start;
  while (i < end) {
    const b0 = buf[i]!;
    if (b0 < 0x80) {
      i++;
      n++;
      continue;
    }
    let need: number;
    let lo = 0x80;
    let hi = 0xbf;
    if (b0 >= 0xc2 && b0 <= 0xdf) need = 1;
    else if (b0 >= 0xe0 && b0 <= 0xef) {
      need = 2;
      if (b0 === 0xe0) lo = 0xa0;
      else if (b0 === 0xed) hi = 0x9f;
    } else if (b0 >= 0xf0 && b0 <= 0xf4) {
      need = 3;
      if (b0 === 0xf0) lo = 0x90;
      else if (b0 === 0xf4) hi = 0x8f;
    } else return -1;
    if (i + need >= end) return -1;
    const b1 = buf[i + 1]!;
    if (b1 < lo || b1 > hi) return -1;
    for (let k = 2; k <= need; k++) {
      const bk = buf[i + k]!;
      if (bk < 0x80 || bk > 0xbf) return -1;
    }
    i += need + 1;
    n++;
  }
  return n;
}

// arrayElementSpans walks an inline array body buf[start, end) (spec/design/array.md §4) and collects
// its non-NULL element bodies as flattened [start, end) offset pairs into buf; it returns whether any
// element is NULL. null unless the body is exactly well-formed — the header the decoder accepts, every
// element in bounds, text elements valid UTF-8, and no trailing bytes — so a malformed body falls back
// to the decoder, which reports it.
export function arrayElementSpans(
  buf: Uint8Array,
  start: number,
  end: number,
  elem: ElemCodec,
  spans: number[],
): boolean | null {
  spans.length = 0;
  if (end - start < 2) return null;
  const ndim = buf[start]!;
  const flags = buf[start + 1]!;
  if ((flags & ~0x01) !== 0) return null;
  let pos = start + 2;
  if (ndim === 0) return pos === end ? false : null;
  if (ndim > 6) return null;
  let n = 1;
  for (let d = 0; d < ndim; d++) {
    if (pos + 8 > end) return null;
    const dim =
      ((buf[pos]! << 24) | (buf[pos + 1]! << 16) | (buf[pos + 2]! << 8) | buf[pos + 3]!) >>> 0;
    n *= dim;
    // More elements than the body has bits (each NULL takes a bitmap bit, each present element at
    // least a byte) is malformed; the bound also keeps n an exact integer.
    if (n > (end - start) * 8) return null;
    pos += 8; // length + lower bound
  }
  const hasNulls = (flags & 0x01) !== 0;
  let bitmap = 0;
  if (hasNulls) {
    const nb = Math.ceil(n / 8);
    if (nb > end - pos) return null;
    bitmap = pos;
    pos += nb;
  }
  let anyNull = false;
  for (let e = 0; e < n; e++) {
    if (hasNulls && (buf[bitmap + (e >> 3)]! & (0x80 >> (e % 8))) !== 0) {
      anyNull = true;
      continue;
    }
    let elemEnd: number;
    if (elem.width === 0) {
      if (pos + 2 > end) return null;
      elemEnd = pos + 2 + ((buf[pos]! << 8) | buf[pos + 1]!);
    } else {
      elemEnd = pos + elem.width;
    }
    if (elemEnd > end) return null;
    if (elem.text && utf8CodePoints(buf, pos + 2, elemEnd) < 0) return null;
    spans.push(pos, elemEnd);
    pos = elemEnd;
  }
  return pos === end ? anyNull : null;
}

// constantOperand evaluates a row-independent constant operand once: its value and the charge the
// evaluator accrues evaluating it (replayed per row). Only literal shapes qualify — a text / bytea /
// integer / NULL literal, a folded array constant, a bind parameter, or a flat ARRAY[...] of those —
// whose evaluation charges at most the constructor's operator_eval and reserves nothing.
function constantOperand(e: RExpr, env: EvalEnv): { value: Value; charge: bigint } | null {
  const literal = (x: RExpr): boolean =>
    x.kind === "constText" ||
    x.kind === "constBytea" ||
    x.kind === "constInt" ||
    x.kind === "constNull" ||
    x.kind === "constArray" ||
    x.kind === "param";
  if (e.kind === "array") {
    if (e.nested || !e.elements.every(literal)) return null;
  } else if (!literal(e)) {
    return null;
  }
  const m = new Meter();
  try {
    const value = evalExpr(e, [], env, m);
    return { value, charge: m.accrued };
  } catch {
    return null;
  }
}

function compileSpanAtom(
  e: RExpr,
  colTypes: ColType[],
  mask: boolean[],
  env: EvalEnv,
): SpanAtom | null {
  const column = (x: RExpr): number | null =>
    x.kind === "column" &&
    x.index >= 0 &&
    x.index < mask.length &&
    mask[x.index] === true &&
    x.index < colTypes.length
      ? x.index
      : null;
  switch (e.kind) {
    case "isNull": {
      const col = column(e.operand);
      // A composite's IS NULL is the all-fields rule (composite.md §5) — not nullness.
      if (col === null || colTypes[col]!.kind === "composite") return null;
      return { expr: e, col, kind: "nullTest", negated: e.negated };
    }
    case "compare": {
      if (e.op !== "eq" && e.op !== "ne") return null;
      const lc = column(e.lhs);
      const rc = column(e.rhs);
      if ((lc === null) === (rc === null)) return null;
      const col = (lc ?? rc)!;
      const k = constantOperand(lc !== null ? e.rhs : e.lhs, env);
      if (k === null) return null;
      const t = colTypes[col]!;
      if (t.kind !== "scalar") return null;
      const base = operatorCost(e.op) + k.charge;
      const negated = e.op === "ne";
      if (isText(t.scalar) && k.value.kind === "text") {
        const bytes = UTF8.encode(k.value.text);
        return {
          expr: e,
          col,
          kind: "bytesEq",
          negated,
          text: true,
          k: bytes,
          kWidth: utf8CodePoints(bytes, 0, bytes.length),
          base,
        };
      }
      if (isBytea(t.scalar) && k.value.kind === "bytea") {
        const bytes = k.value.bytes;
        return {
          expr: e,
          col,
          kind: "bytesEq",
          negated,
          text: false,
          k: bytes,
          kWidth: bytes.length,
          base,
        };
      }
      return null;
    }
    case "arrayFunc": {
      if (e.args.length !== 2) return null;
      const c0 = column(e.args[0]!);
      const c1 = column(e.args[1]!);
      if ((c0 === null) === (c1 === null)) return null;
      const colFirst = c0 !== null;
      const col = (c0 ?? c1)!;
      let op: ArrayOpKind;
      if ((e.func === "contains" && colFirst) || (e.func === "contained_by" && !colFirst))
        op = "colContainsK";
      else if ((e.func === "contains" && !colFirst) || (e.func === "contained_by" && colFirst))
        op = "kContainsCol";
      else if (e.func === "overlaps") op = "overlaps";
      else return null;
      const t = colTypes[col]!;
      if (t.kind !== "array" || t.elem.kind !== "scalar") return null;
      const es = t.elem.scalar;
      let elem: ElemCodec;
      if (isText(es)) elem = { width: 0, text: true };
      else if (isBytea(es)) elem = { width: 0, text: false };
      else if (isInteger(es)) elem = { width: widthBytes(es), text: false };
      else return null;
      const k = constantOperand(colFirst ? e.args[1]! : e.args[0]!, env);
      if (k === null || k.value.kind !== "array") return null;
      const kElems: Uint8Array[] = [];
      let kHasNull = false;
      for (const v of k.value.elements) {
        if (v.kind === "null") {
          kHasNull = true;
          continue;
        }
        let fits: boolean;
        if (isText(es) && v.kind === "text") fits = UTF8.encode(v.text).length <= 0xffff;
        else if (isBytea(es) && v.kind === "bytea") fits = v.bytes.length <= 0xffff;
        else if (isInteger(es) && v.kind === "int") fits = inRange(es, v.int);
        else fits = false;
        if (!fits) return null;
        kElems.push(encodeValue(t.elem, v).subarray(1));
      }
      return {
        expr: e,
        col,
        kind: "arrayOp",
        op,
        elem,
        kElems,
        kHasNull,
        base: COSTS.operatorEval + k.charge,
        spans: [],
      };
    }
    default:
      return null;
  }
}

export function readLeafColumn(n: PNode, i: number, c: number): Value {
  return runSync(readLeafColumnSteps(n, i, c));
}

export function laneKeep(
  f: LaneFilter,
  n: PNode,
  i: number,
  scratch: ScratchRow,
  env: EvalEnv,
  m: Meter,
): boolean {
  return runSync(laneKeepSteps(f, n, i, scratch, synchronousEvalEnv(env), m));
}

export function spanEvalFilter(
  p: SpanPred,
  n: PNode,
  i: number,
  scratch: ScratchRow,
  env: EvalEnv,
  m: Meter,
): boolean {
  return runSync(spanEvalFilterSteps(p, n, i, scratch, synchronousEvalEnv(env), m));
}
