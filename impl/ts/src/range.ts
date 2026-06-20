// Range types (spec/design/ranges.md): the six built-in PostgreSQL range types as a structural
// container over a scalar element. This file holds the parts the cores hand-write (CLAUDE.md §5,
// the codec/comparator/text-I/O are not codegen'd): the RANGES descriptor lookup, the text
// input/output (parseRangeText/rangeOut), and the canonicalization / empty-normalization / order
// check that produce a CANONICAL stored value (§4). The type-set facts come from the codegen'd
// RANGES table (ranges_gen.ts). The value model is the `range` Value kind; bounds are element Values.

import { encodeInt } from "./encoding.ts";
import { type RangeDesc, RANGES } from "./ranges_gen.ts";
import { canonicalName, scalarTypeFromName, type ScalarType } from "./types.ts";
import { emptyRangeValue, rangeValue, render, type Value } from "./value.ts";
import { engineError } from "./errors.ts";

// rangeByName looks up a range type by name (case-insensitive), matching the canonical id or any
// alias (int4range → i32range). undefined if name is not one of the six range types.
export function rangeByName(name: string): RangeDesc | undefined {
  const lname = name.toLowerCase();
  return RANGES.find((r) => r.id === lname || r.aliases.includes(lname));
}

// rangeNameForElement returns the canonical range type name for an element scalar (i32 → i32range),
// or undefined if the element has no built-in range type. Inverse of elementScalar.
export function rangeNameForElement(elem: ScalarType): string | undefined {
  const ename = canonicalName(elem);
  return RANGES.find((r) => r.element === ename)?.id;
}

// elementScalar returns the element scalar type of a range descriptor (i32range → i32). The
// descriptor's element is always a valid scalar id, so the lookup never fails.
export function elementScalar(desc: RangeDesc): ScalarType {
  return scalarTypeFromName(desc.element) as ScalarType;
}

// rangeForElement returns the range descriptor whose element is `elem` (i32 → the i32range
// descriptor), or undefined if the scalar has no built-in range type. Used by the storage/codec
// paths that hold a resolved element ScalarType (a range column's range ColType element) and need
// the descriptor's discreteness / canonicalization rule. Inverse of elementScalar.
export function rangeForElement(elem: ScalarType): RangeDesc | undefined {
  const ename = canonicalName(elem);
  return RANGES.find((r) => r.element === ename);
}

// --- text input ------------------------------------------------------------

// ParsedRange is a range literal parsed lexically (before element coercion): the bracket
// inclusivity, the two bound texts (null = an empty/omitted bound = infinite), and the empty flag.
export type ParsedRange = {
  empty: boolean;
  lower: string | null;
  upper: string | null;
  lowerInc: boolean;
  upperInc: boolean;
};

function malformedRange(input: string): EngineErrorLike {
  return engineError("invalid_text_representation", `malformed range literal: "${input}"`);
}
type EngineErrorLike = ReturnType<typeof engineError>;

// parseRangeText parses a range text literal into its lexical parts (spec/design/ranges.md §5), PG
// range_in: optional surrounding whitespace; `empty` (case-insensitive); or [/( lower , upper )/]
// with each bound possibly double-quoted ("" / \ escapes) and an empty bound meaning infinite. A
// malformed literal throws 22P02.
export function parseRangeText(input: string): ParsedRange {
  const s = input.trim();
  if (s.toLowerCase() === "empty") {
    return { empty: true, lower: null, upper: null, lowerInc: false, upperInc: false };
  }
  if (s.length === 0) throw malformedRange(input);
  let lowerInc: boolean;
  if (s[0] === "[") lowerInc = true;
  else if (s[0] === "(") lowerInc = false;
  else throw malformedRange(input);

  let pos = 1;
  const low = scanBound(s, pos);
  if (low === null) throw malformedRange(input);
  pos = low.next;
  if (pos >= s.length || s[pos] !== ",") throw malformedRange(input);
  pos++; // the comma
  const up = scanBound(s, pos);
  if (up === null) throw malformedRange(input);
  pos = up.next;
  if (pos !== s.length - 1) throw malformedRange(input);
  let upperInc: boolean;
  if (s[pos] === "]") upperInc = true;
  else if (s[pos] === ")") upperInc = false;
  else throw malformedRange(input);

  return { empty: false, lower: low.bound, upper: up.bound, lowerInc, upperInc };
}

// scanBound scans one bound starting at offset start, returning { bound, next } where bound is null
// for an empty (infinite) bound and next points at the delimiter. A quoted bound ("…") unescapes ""
// → " and \x → x; an unquoted bound runs to the next top-level , / ) / ]. null = a malformed literal
// (an unterminated quote).
function scanBound(s: string, start: number): { bound: string | null; next: number } | null {
  if (start >= s.length) return null;
  if (s[start] === '"') {
    let out = "";
    let i = start + 1;
    for (;;) {
      if (i >= s.length) return null; // unterminated quote
      const c = s[i];
      if (c === '"') {
        if (i + 1 < s.length && s[i + 1] === '"') {
          out += '"';
          i += 2;
        } else {
          return { bound: out, next: i + 1 };
        }
      } else if (c === "\\") {
        if (i + 1 >= s.length) return null;
        out += s[i + 1];
        i += 2;
      } else {
        out += c;
        i++;
      }
    }
  }
  let i = start;
  while (i < s.length && s[i] !== "," && s[i] !== ")" && s[i] !== "]") i++;
  const raw = s.slice(start, i).trim();
  return { bound: raw === "" ? null : raw, next: i };
}

// --- canonicalization ------------------------------------------------------

// rangeElemCmp compares two element bound values of the same range element type (-1/0/1). The six
// element types store their orderable value as a bigint (int/date/timestamps) or a Decimal.
export function rangeElemCmp(a: Value, b: Value): number {
  if (a.kind === "decimal" && b.kind === "decimal") return a.dec.cmpValue(b.dec);
  const av = boundInt(a);
  const bv = boundInt(b);
  return av < bv ? -1 : av > bv ? 1 : 0;
}

function boundInt(v: Value): bigint {
  switch (v.kind) {
    case "int":
      return v.int;
    case "date":
      return v.days;
    case "timestamp":
    case "timestamptz":
      return v.micros;
    default:
      // Same-element bounds only reach here; any other kind is an engine invariant violation.
      return 0n;
  }
}

const ELEM_MAX_FINITE: Record<string, bigint> = {
  i16: 32767n,
  i32: 2147483647n,
  i64: 9223372036854775807n,
  // date's i32::MAX is the +infinity sentinel, so the finite max is one below it.
  date: 2147483646n,
};

// incrementDiscrete steps a discrete bound value up by one unit (the canonicalization +1): an
// integer +1 or a date +1 day. A step past the element domain throws 22003.
function incrementDiscrete(v: Value, elem: ScalarType): Value {
  const max = ELEM_MAX_FINITE[elem] ?? 9223372036854775807n;
  const cur = boundInt(v);
  if (cur >= max) {
    throw engineError("numeric_value_out_of_range", `value out of range for type ${elem}`);
  }
  if (v.kind === "date") return { kind: "date", days: cur + 1n };
  return { kind: "int", int: cur + 1n };
}

// finalizeRange builds a CANONICAL range Value from coerced bound values (spec/design/ranges.md §4):
// the order check (lower > upper → 22000), discrete canonicalization to `[)` (throwing 22003 on a
// step past the domain), and empty normalization (lower == upper not-both-inclusive → empty). A null
// bound is infinite.
export function finalizeRange(
  desc: RangeDesc,
  lower: Value | null,
  upper: Value | null,
  lowerInc: boolean,
  upperInc: boolean,
): Value {
  const elem = elementScalar(desc);
  // Order check: two finite bounds must be lower ≤ upper.
  if (lower !== null && upper !== null && rangeElemCmp(lower, upper) > 0) {
    throw engineError(
      "data_exception",
      "range lower bound must be less than or equal to range upper bound",
    );
  }
  if (desc.discrete) {
    // Canonical `[)`: an exclusive finite lower steps up to inclusive; an inclusive finite upper
    // steps up to exclusive. Infinite bounds stay exclusive.
    if (lower !== null && !lowerInc) {
      lower = incrementDiscrete(lower, elem);
      lowerInc = true;
    } else if (lower === null) {
      lowerInc = false;
    }
    if (upper !== null && upperInc) {
      upper = incrementDiscrete(upper, elem);
      upperInc = false;
    } else if (upper === null) {
      upperInc = false;
    }
  } else {
    if (lower === null) lowerInc = false;
    if (upper === null) upperInc = false;
  }
  // Empty normalization: equal finite bounds that are not both inclusive contain no points. For
  // discrete ranges the canonical `[)` form already makes a one-point range `[x,x)` land here.
  if (lower !== null && upper !== null && rangeElemCmp(lower, upper) === 0 && !(lowerInc && upperInc)) {
    return emptyRangeValue();
  }
  return rangeValue(lower, upper, lowerInc, upperInc);
}

// parseBoundFlags parses a 2-character range-constructor bounds-flags string (`'[]'`/`'[)'`/`'(]'`/
// `'()'`) into [lowerInc, upperInc] — the 3-arg constructor's third argument
// (spec/design/range-functions.md §2). The lower character is `[` (inclusive) or `(` (exclusive);
// the upper is `]` (inclusive) or `)` (exclusive). Any other string throws 42601 (PG "invalid range
// bound flags"). The caller handles a NULL flags argument separately (22000, before this is reached).
export function parseBoundFlags(s: string): [boolean, boolean] {
  switch (s) {
    case "[]":
      return [true, true];
    case "[)":
      return [true, false];
    case "(]":
      return [false, true];
    case "()":
      return [false, false];
    default:
      throw engineError("syntax_error", "invalid range bound flags");
  }
}

// --- comparison ------------------------------------------------------------

// RangeShape is the structural view of a range value used by the comparator (its empty flag,
// element bounds, and inclusivity). The `range` Value kind has exactly these fields.
type RangeShape = {
  empty: boolean;
  lower: Value | null;
  upper: Value | null;
  lowerInc: boolean;
  upperInc: boolean;
};

// rangeTotalCmp is the PG range_cmp total order over two CANONICAL range values
// (spec/design/ranges.md §6, -1/0/1): `empty` sorts below every non-empty range, then by lower
// bound, then by upper bound. Each bound comparison (cmpBound) accounts for infinity and
// inclusivity. A total order (always a definite result, never 3-valued — unlike composite), and
// consistent with the structural range equality (two canonical ranges are equal iff rangeTotalCmp
// is 0). Shared by value's lt3/gt3 and executor's valueCmp so `<` and `ORDER BY` never disagree.
export function rangeTotalCmp(a: RangeShape, b: RangeShape): number {
  if (a.empty && b.empty) return 0;
  if (a.empty) return -1;
  if (b.empty) return 1;
  const c = cmpBound(a.lower, a.lowerInc, b.lower, b.lowerInc, true);
  if (c !== 0) return c;
  return cmpBound(a.upper, a.upperInc, b.upper, b.upperInc, false);
}

// cmpBound compares two range bounds on the SAME side (lower-vs-lower or upper-vs-upper), PG
// range_cmp_bounds (-1/0/1). The same-side specialization of cmpBounds (both bounds carry the same
// isLower), used by the total order; a null value is the unbounded/infinite bound.
function cmpBound(
  v1: Value | null,
  inc1: boolean,
  v2: Value | null,
  inc2: boolean,
  isLower: boolean,
): number {
  return cmpBounds(v1, inc1, isLower, v2, inc2, isLower);
}

// cmpBounds is the general PG range_cmp_bounds (-1/0/1): compare two range bounds that may be on
// DIFFERENT sides — each carries its own value (null = infinite), inclusivity, and isLower flag (the
// boolean operators RF3 compare a lower against an upper). An infinite LOWER is below everything; an
// infinite UPPER is above everything. For equal finite values only a differing inclusivity breaks the
// tie: the exclusive bound sits just inside on its own side, so an exclusive LOWER sorts after (it
// starts later) and an exclusive UPPER sorts before (it ends earlier). cmpBound (same-side) is the
// lower1 === lower2 case.
function cmpBounds(
  v1: Value | null,
  inc1: boolean,
  lower1: boolean,
  v2: Value | null,
  inc2: boolean,
  lower2: boolean,
): number {
  if (v1 === null && v2 === null) {
    if (lower1 === lower2) return 0;
    return lower1 ? -1 : 1;
  }
  if (v1 === null) return lower1 ? -1 : 1;
  if (v2 === null) return lower2 ? 1 : -1;
  const c = rangeElemCmp(v1, v2);
  if (c !== 0) return c;
  // Equal values: only a differing inclusivity breaks the tie (PG range_cmp_bounds). The exclusive
  // side decides — an exclusive lower sorts after, an exclusive upper before.
  if (inc1 && !inc2) return lower2 ? -1 : 1;
  if (!inc1 && inc2) return lower1 ? 1 : -1;
  return 0;
}

// --- key encoding (spec/design/encoding.md §2.11) --------------------------

// encodeRangeKey is the order-preserving storage-key bytes for a range value
// (spec/design/encoding.md §2.11) — the engine's first container key. It frames the range's shape and
// embeds each finite bound's element key, so that memcmp over the bytes reproduces rangeTotalCmp: a
// leading empty/non-empty discriminator (0x00 empty sorts first, 0x01 non-empty), then the lower
// bound, then the upper bound. Each bound is either a single infinity marker (0x00 = −∞ on the lower
// side, 0x02 = +∞ on the upper — ordered −∞ < finite < +∞) or 0x01 ‖ the element's own
// order-preserving key ‖ an inclusivity byte. elem names the element scalar (the integer codec needs
// the width). Keys never round-trip (the row body holds the full value), so this need only sort.
export function encodeRangeKey(elem: ScalarType, rv: RangeShape): Uint8Array {
  if (rv.empty) return Uint8Array.of(0x00); // empty sorts below every non-empty range; whole key
  const out: number[] = [0x01];
  pushRangeBound(out, elem, rv.lower, rv.lowerInc, true);
  pushRangeBound(out, elem, rv.upper, rv.upperInc, false);
  return Uint8Array.from(out);
}

// pushRangeBound appends one bound of a non-empty range. An infinite bound is a single marker
// (−∞ = 0x00 lower, +∞ = 0x02 upper); a finite bound is 0x01 ‖ the element key ‖ a one-byte
// inclusivity tie-break (PG range_cmp_bounds): on the LOWER side an inclusive bound sorts before an
// exclusive one, on the UPPER side an exclusive bound sorts before an inclusive one — i.e. the byte is
// 0x00 when inc === isLower, else 0x01.
function pushRangeBound(
  out: number[],
  elem: ScalarType,
  v: Value | null,
  inc: boolean,
  isLower: boolean,
): void {
  if (v === null) {
    out.push(isLower ? 0x00 : 0x02);
    return;
  }
  out.push(0x01);
  for (const b of encodeRangeElem(elem, v)) out.push(b);
  out.push(inc === isLower ? 0x00 : 0x01);
}

// encodeRangeElem encodes one range bound value's element key. A range element is one of the six
// scalar subtypes (i32/i64/decimal/date/timestamp/timestamptz); decimal uses decimal-order-preserving
// (§2.5), the rest the int-be-signflip / day / instant codec.
function encodeRangeElem(elem: ScalarType, v: Value): Uint8Array {
  if (v.kind === "decimal") return v.dec.encodeKey();
  if (v.kind === "date") return encodeInt(elem, v.days);
  if (v.kind === "timestamp" || v.kind === "timestamptz") return encodeInt(elem, v.micros);
  if (v.kind === "int") return encodeInt(elem, v.int);
  throw new Error("a range element is i32/i64/decimal/date/timestamp/timestamptz");
}

// --- boolean operators (RF3, spec/design/range-functions.md §3) -------------
// The eight PG range boolean operators, each a definite boolean over CANONICAL range values (never
// 3-valued — like the total order, unlike composite; a NULL operand is short-circuited by the
// evaluator before these are called). Containment/overlap/positional/adjacent, built on the general
// bound comparison cmpBounds. Empty-range edges follow PG: the empty range contains nothing and is
// contained by everything; it overlaps nothing and is neither before/after/adjacent to anything.

// rangeContainsElem is `r @> e` — does range r contain element value e (PG range_contains_elem). e is
// already the range's element type (the resolver coerced it). The empty range contains nothing.
export function rangeContainsElem(r: RangeShape, e: Value): boolean {
  if (r.empty) return false;
  if (r.lower !== null) {
    const c = rangeElemCmp(e, r.lower);
    if (c < 0) return false;
    if (c === 0 && !r.lowerInc) return false;
  }
  if (r.upper !== null) {
    const c = rangeElemCmp(e, r.upper);
    if (c > 0) return false;
    if (c === 0 && !r.upperInc) return false;
  }
  return true;
}

// rangeContains is `a @> b` — does range a contain range b (PG range_contains): the empty range is
// contained by everything, and a non-empty b is contained only when a's lower bound is ≤ b's and a's
// upper bound is ≥ b's (each in the cmpBounds sense).
export function rangeContains(a: RangeShape, b: RangeShape): boolean {
  if (b.empty) return true;
  if (a.empty) return false;
  return (
    cmpBounds(a.lower, a.lowerInc, true, b.lower, b.lowerInc, true) <= 0 &&
    cmpBounds(a.upper, a.upperInc, false, b.upper, b.upperInc, false) >= 0
  );
}

// rangeOverlaps is `a && b` — do ranges a and b overlap, sharing at least one point (PG
// range_overlaps). The empty range overlaps nothing. They overlap iff one range's lower bound lies
// within the other.
export function rangeOverlaps(a: RangeShape, b: RangeShape): boolean {
  if (a.empty || b.empty) return false;
  return lowerWithin(a, b) || lowerWithin(b, a);
}

// lowerWithin reports whether the lower bound of x lies within y (x.lower ≥ y.lower and x.lower ≤
// y.upper, in the cmpBounds sense) — the half-test of rangeOverlaps.
function lowerWithin(x: RangeShape, y: RangeShape): boolean {
  return (
    cmpBounds(x.lower, x.lowerInc, true, y.lower, y.lowerInc, true) >= 0 &&
    cmpBounds(x.lower, x.lowerInc, true, y.upper, y.upperInc, false) <= 0
  );
}

// rangeBefore is `a << b` — is a strictly left of b, every point of a below every point of b (PG
// range_before): a's upper bound is below b's lower bound. The empty range is never strictly
// left/right of anything.
export function rangeBefore(a: RangeShape, b: RangeShape): boolean {
  if (a.empty || b.empty) return false;
  return cmpBounds(a.upper, a.upperInc, false, b.lower, b.lowerInc, true) < 0;
}

// rangeAfter is `a >> b` — is a strictly right of b (PG range_after), i.e. b << a.
export function rangeAfter(a: RangeShape, b: RangeShape): boolean {
  return rangeBefore(b, a);
}

// rangeOverleft is `a &< b` — does a not extend to the right of b (a.upper ≤ b.upper; PG
// range_overleft).
export function rangeOverleft(a: RangeShape, b: RangeShape): boolean {
  if (a.empty || b.empty) return false;
  return cmpBounds(a.upper, a.upperInc, false, b.upper, b.upperInc, false) <= 0;
}

// rangeOverright is `a &> b` — does a not extend to the left of b (a.lower ≥ b.lower; PG
// range_overright).
export function rangeOverright(a: RangeShape, b: RangeShape): boolean {
  if (a.empty || b.empty) return false;
  return cmpBounds(a.lower, a.lowerInc, true, b.lower, b.lowerInc, true) >= 0;
}

// rangeAdjacent is `a -|- b` — are a and b adjacent: they touch at exactly one boundary value with
// complementary inclusivity (no gap, no overlap; PG range_adjacent). Over the CANONICAL representation
// this is just "a's upper bound value equals b's lower bound value, exactly one inclusive, or vice
// versa" — the discrete `[)` canonicalization already folded the integer/date step into the bounds.
export function rangeAdjacent(a: RangeShape, b: RangeShape): boolean {
  if (a.empty || b.empty) return false;
  return (
    boundsTouch(a.upper, a.upperInc, b.lower, b.lowerInc) ||
    boundsTouch(b.upper, b.upperInc, a.lower, a.lowerInc)
  );
}

// boundsTouch reports whether a finite upper bound and a finite lower bound meet at one point with
// complementary inclusivity (exactly one includes the shared value) — the adjacency condition. An
// infinite bound never touches.
function boundsTouch(
  upper: Value | null,
  upperInc: boolean,
  lower: Value | null,
  lowerInc: boolean,
): boolean {
  if (upper === null || lower === null) return false;
  return rangeElemCmp(upper, lower) === 0 && upperInc !== lowerInc;
}

// --- set operators (RF4, spec/design/range-functions.md §4) -----------------
// The three set operators `+`/`*`/`-` and `range_merge`, over CANONICAL range values (PG
// range_union/range_intersect/range_minus, rangetypes.c). They reuse the same cmpBound/cmpBounds
// bound comparison as the boolean operators above; the result bounds are taken from the operands'
// (already-canonical) bounds, so no re-canonicalization is needed — only makeRange's
// empty-normalization applies (PG's make_range minus the canonicalize step the operands satisfy).
// `+` and `-` raise 22000 when the result would not be a single contiguous range; `*` and
// range_merge never error.

// makeRange assembles a range Value from selected bounds (PG make_range, minus the discrete
// canonicalize step the operands already satisfy): force an infinite bound's inclusivity off, then
// collapse to `empty` when the bounds cross (lower > upper) or meet at one value without both being
// inclusive.
function makeRange(
  lower: Value | null,
  upper: Value | null,
  lowerInc: boolean,
  upperInc: boolean,
): Value {
  if (lower === null) lowerInc = false;
  if (upper === null) upperInc = false;
  if (lower !== null && upper !== null) {
    const c = rangeElemCmp(lower, upper);
    if (c > 0) return emptyRangeValue();
    if (c === 0 && !(lowerInc && upperInc)) return emptyRangeValue();
  }
  return rangeValue(lower, upper, lowerInc, upperInc);
}

// rangeUnion is `a + b` (union) and `range_merge(a, b)` — the smallest single range covering both
// (PG range_union_internal). With `strict` (the `+` operator) the two ranges must overlap or be
// adjacent, else the union would span a gap and is 22000; range_merge (strict = false) spans the gap
// silently. An empty operand yields the other unchanged.
export function rangeUnion(a: RangeShape, b: RangeShape, strict: boolean): Value {
  if (a.empty) return rangeShapeValue(b);
  if (b.empty) return rangeShapeValue(a);
  if (strict && !rangeOverlaps(a, b) && !rangeAdjacent(a, b)) {
    throw engineError("data_exception", "result of range union would not be contiguous");
  }
  // result lower = the lesser lower bound; result upper = the greater upper bound.
  let lower: Value | null;
  let lowerInc: boolean;
  if (cmpBound(a.lower, a.lowerInc, b.lower, b.lowerInc, true) < 0) {
    lower = a.lower;
    lowerInc = a.lowerInc;
  } else {
    lower = b.lower;
    lowerInc = b.lowerInc;
  }
  let upper: Value | null;
  let upperInc: boolean;
  if (cmpBound(a.upper, a.upperInc, b.upper, b.upperInc, false) > 0) {
    upper = a.upper;
    upperInc = a.upperInc;
  } else {
    upper = b.upper;
    upperInc = b.upperInc;
  }
  return rangeValue(lower, upper, lowerInc, upperInc);
}

// rangeIntersect is `a * b` (intersection) — the overlap of two ranges (PG range_intersect_internal),
// or `empty` when they do not overlap (disjoint, merely adjacent, or either operand empty). Never
// errors.
export function rangeIntersect(a: RangeShape, b: RangeShape): Value {
  if (a.empty || b.empty || !rangeOverlaps(a, b)) return emptyRangeValue();
  // result lower = the greater lower bound; result upper = the lesser upper bound.
  let lower: Value | null;
  let lowerInc: boolean;
  if (cmpBound(a.lower, a.lowerInc, b.lower, b.lowerInc, true) >= 0) {
    lower = a.lower;
    lowerInc = a.lowerInc;
  } else {
    lower = b.lower;
    lowerInc = b.lowerInc;
  }
  let upper: Value | null;
  let upperInc: boolean;
  if (cmpBound(a.upper, a.upperInc, b.upper, b.upperInc, false) <= 0) {
    upper = a.upper;
    upperInc = a.upperInc;
  } else {
    upper = b.upper;
    upperInc = b.upperInc;
  }
  return makeRange(lower, upper, lowerInc, upperInc);
}

// rangeMinus is `a - b` (difference) — the part of `a` not covered by `b` (PG range_minus_internal).
// 22000 when `b` lies strictly inside `a` and would split it into two pieces (a non-contiguous
// result). An empty operand, or a `b` disjoint from `a`, yields `a` unchanged.
export function rangeMinus(a: RangeShape, b: RangeShape): Value {
  if (a.empty || b.empty) return rangeShapeValue(a);
  const cmpL1L2 = cmpBounds(a.lower, a.lowerInc, true, b.lower, b.lowerInc, true);
  const cmpL1U2 = cmpBounds(a.lower, a.lowerInc, true, b.upper, b.upperInc, false);
  const cmpU1L2 = cmpBounds(a.upper, a.upperInc, false, b.lower, b.lowerInc, true);
  const cmpU1U2 = cmpBounds(a.upper, a.upperInc, false, b.upper, b.upperInc, false);

  // `b` strictly inside `a` (a.lower < b.lower and a.upper > b.upper): removing it leaves two disjoint
  // pieces — a non-contiguous result.
  if (cmpL1L2 < 0 && cmpU1U2 > 0) {
    throw engineError("data_exception", "result of range difference would not be contiguous");
  }
  // `a` and `b` do not overlap: `a` is unchanged.
  if (cmpL1U2 > 0 || cmpU1L2 < 0) return rangeShapeValue(a);
  // `a` is wholly within `b`: nothing remains.
  if (cmpL1L2 >= 0 && cmpU1U2 <= 0) return emptyRangeValue();
  // `b` covers the right part of `a`: keep `[a.lower, b.lower)` — `b`'s lower bound becomes the
  // result's upper bound, so its inclusivity flips.
  if (cmpL1L2 <= 0 && cmpU1L2 >= 0 && cmpU1U2 <= 0) {
    return makeRange(a.lower, b.lower, a.lowerInc, !b.lowerInc);
  }
  // `b` covers the left part of `a`: keep `[b.upper, a.upper)` — `b`'s upper bound becomes the
  // result's lower bound, so its inclusivity flips.
  if (cmpL1L2 >= 0 && cmpU1U2 >= 0 && cmpL1U2 <= 0) {
    return makeRange(b.upper, a.upper, !b.upperInc, a.upperInc);
  }
  throw new Error("unexpected case in rangeMinus");
}

// rangeShapeValue rebuilds a Value from a RangeShape (the union/difference "yields the other operand
// unchanged" paths) — the Rust `clone()` of the unchanged operand.
function rangeShapeValue(r: RangeShape): Value {
  if (r.empty) return emptyRangeValue();
  return rangeValue(r.lower, r.upper, r.lowerInc, r.upperInc);
}

// --- text output -----------------------------------------------------------

// rangeOut renders a range value as PG range_out (spec/design/ranges.md §5): `empty`, or
// [(lower,upper)] with the bound omitted for infinite and double-quoted where the element's text has
// a special character (so a tsrange bound's space is quoted, a daterange bound is bare).
export function rangeOut(v: Value & { kind: "range" }): string {
  if (v.empty) return "empty";
  let out = v.lowerInc ? "[" : "(";
  if (v.lower !== null) out += quoteBound(render(v.lower));
  out += ",";
  if (v.upper !== null) out += quoteBound(render(v.upper));
  out += v.upperInc ? "]" : ")";
  return out;
}

const RANGE_SPECIAL = new Set([" ", "\t", "\n", "\r", "\f", "\v", ",", "[", "]", "(", ")", '"', "\\"]);

// quoteBound double-quotes a bound's rendered text if it needs it (PG range_out quoting): empty, or
// containing whitespace or any of , [ ] ( ) " \. Inside, " → "" and \ → \\.
function quoteBound(text: string): string {
  let needs = text.length === 0;
  for (const c of text) {
    if (RANGE_SPECIAL.has(c)) {
      needs = true;
      break;
    }
  }
  if (!needs) return text;
  let out = '"';
  for (const c of text) {
    if (c === '"' || c === "\\") out += "\\";
    out += c;
  }
  return out + '"';
}
