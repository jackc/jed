// Range types (spec/design/ranges.md): the six built-in PostgreSQL range types as a structural
// container over a scalar element. This file holds the parts the cores hand-write (CLAUDE.md §5,
// the codec/comparator/text-I/O are not codegen'd): the RANGES descriptor lookup, the text
// input/output (parseRangeText/rangeOut), and the canonicalization / empty-normalization / order
// check that produce a CANONICAL stored value (§4). The type-set facts come from the codegen'd
// RANGES table (ranges_gen.ts). The value model is the `range` Value kind; bounds are element Values.

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

// cmpBound compares two range bounds on the same side (lower-vs-lower or upper-vs-upper), PG
// range_cmp_bounds (-1/0/1). A null value is the unbounded/infinite bound: an infinite lower is
// below any finite lower, an infinite upper is above any finite upper. For equal finite values the
// inclusivity breaks the tie, and the direction depends on the side: a lower bound sorts
// inclusive-before-exclusive (`[1` < `(1`), an upper bound sorts exclusive-before-inclusive
// (`1)` < `1]`). isLower selects that direction.
function cmpBound(
  v1: Value | null,
  inc1: boolean,
  v2: Value | null,
  inc2: boolean,
  isLower: boolean,
): number {
  if (v1 === null && v2 === null) return 0;
  if (v1 === null) return isLower ? -1 : 1;
  if (v2 === null) return isLower ? 1 : -1;
  const c = rangeElemCmp(v1, v2);
  if (c !== 0) return c;
  // Equal values: an exclusive lower sorts after an inclusive lower; an exclusive upper sorts
  // before an inclusive upper (the rest fall out of the both-equal cases).
  if (inc1 === inc2) return 0;
  if (!inc1 && inc2) return isLower ? 1 : -1;
  return isLower ? -1 : 1;
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
