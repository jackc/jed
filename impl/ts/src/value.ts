// Runtime values and three-valued (Kleene) logic.
//
// A Value is SQL NULL, an integer, or a boolean. Integers are held as `bigint`
// regardless of declared column type (so int64 is exact — JS `number` cannot represent
// the full int64 range); the declared type governs range checks and key-encoding width.
// boolean is expression-only (spec/design/types.md §1): a "bool" Value is produced by
// comparisons and connectives and can be projected/rendered, but is never stored; a NULL
// boolean (unknown) is the "null" Value, so {true, false, NULL} is the three-valued domain.

export type Value =
  | { kind: "null" }
  | { kind: "int"; int: bigint }
  | { kind: "bool"; value: boolean }
  // The first stored non-integer value; compares by the C collation (UTF-8 byte /
  // code-point order — spec/design/types.md §11). NOT compared with JS `<`/localeCompare,
  // which use UTF-16 code-unit order and disagree above U+FFFF (see compareTextC below).
  | { kind: "text"; text: string };

// intValue builds a non-null integer value.
export function intValue(n: bigint): Value {
  return { kind: "int", int: n };
}

// nullValue builds a NULL value.
export function nullValue(): Value {
  return { kind: "null" };
}

// boolValue builds a boolean value.
export function boolValue(b: boolean): Value {
  return { kind: "bool", value: b };
}

// textValue builds a non-null text value.
export function textValue(s: string): Value {
  return { kind: "text", text: s };
}

// compareTextC compares two strings by the `C` collation: the lexicographic order of
// their UTF-8 byte encodings, which for UTF-8 equals Unicode code-point order. Returns
// <0, 0, >0. THIS IS THE CROSS-CORE DETERMINISM TRAP (CLAUDE.md §8, spec/design/types.md
// §11): JS string `<` and localeCompare compare by UTF-16 CODE UNITS, which disagree with
// UTF-8 byte order for any character above U+FFFF (e.g. U+F900 vs U+1F600). Rust (str Ord)
// and Go (string compare) are byte order natively; TS must encode to UTF-8 and memcmp to
// match them. Pinned by the astral-char case in spec/conformance/suites/types/text.test.
const TEXT_ENCODER = new TextEncoder();
export function compareTextC(a: string, b: string): number {
  if (a === b) return 0; // fast path: equal strings encode to equal bytes
  const ba = TEXT_ENCODER.encode(a);
  const bb = TEXT_ENCODER.encode(b);
  const n = ba.length < bb.length ? ba.length : bb.length;
  for (let i = 0; i < n; i++) {
    if (ba[i] !== bb[i]) return ba[i]! < bb[i]! ? -1 : 1;
  }
  return ba.length === bb.length ? 0 : ba.length < bb.length ? -1 : 1;
}

// ThreeValued is the result of a three-valued comparison (CLAUDE.md §4):
// TRUE / FALSE / UNKNOWN. UNKNOWN arises whenever a NULL participates.
export type ThreeValued = "true" | "false" | "unknown";

// isTrue reports whether a WHERE expression keeps a row: only boolean TRUE selects;
// FALSE and NULL/unknown both reject (CLAUDE.md §4, Kleene).
export function isTrue(v: Value): boolean {
  return v.kind === "bool" && v.value;
}

function bool3(b: boolean): ThreeValued {
  return b ? "true" : "false";
}

// render formats for conformance output: integers as shortest decimal, booleans as the
// canonical "true"/"false", NULL (including a NULL/unknown boolean) as the literal
// "NULL" (spec/design/conformance.md §1; the canonical spelling is a §8 decision).
// BigInt#toString gives the plain decimal with no `n` suffix, matching Go's FormatInt.
export function render(v: Value): string {
  switch (v.kind) {
    case "null":
      return "NULL";
    case "bool":
      return v.value ? "true" : "false";
    case "text":
      return v.text;
    default:
      return v.int.toString();
  }
}

// eq3 is three-valued equality. NULL compared with anything (including NULL) is
// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers compare by
// value (all integer types promote losslessly into the common bigint); text by the C
// collation (UTF-8 byte order — string equality is exact for ===). A mixed int/text pair
// never reaches here (the resolver rejects it, 42804); any other variant pair is a NULL.
export function eq3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  if (a.kind === "text" && b.kind === "text") return bool3(a.text === b.text);
  if (a.kind === "int" && b.kind === "int") return bool3(a.int === b.int);
  return "unknown";
}

// lt3 is the three-valued ordering predicate a < b (text by C collation = UTF-8 byte order).
export function lt3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  if (a.kind === "text" && b.kind === "text") return bool3(compareTextC(a.text, b.text) < 0);
  if (a.kind === "int" && b.kind === "int") return bool3(a.int < b.int);
  return "unknown";
}

// gt3 is the three-valued ordering predicate a > b (text by C collation = UTF-8 byte order).
export function gt3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  if (a.kind === "text" && b.kind === "text") return bool3(compareTextC(a.text, b.text) > 0);
  if (a.kind === "int" && b.kind === "int") return bool3(a.int > b.int);
  return "unknown";
}

// notDistinctFrom is NULL-safe equality — the `IS NOT DISTINCT FROM` primitive
// (CLAUDE.md §4, spec/design/functions.md §3). NULL is a comparable value, not a poison:
// two NULLs are "not distinct" (the same), a NULL and a present value are distinct, and
// two present integers compare by value. The answer is always definite — there is no
// UNKNOWN here, which is the whole point of the operator. `IS DISTINCT FROM` is the
// negation of this. (The resolver guarantees integer/NULL operands, so non-null values
// reduce to eq3, which is definite when neither side is NULL.)
export function notDistinctFrom(a: Value, b: Value): boolean {
  if (a.kind === "null" || b.kind === "null") return a.kind === "null" && b.kind === "null";
  return eq3(a, b) === "true";
}

// --- boolean Value <-> ThreeValued bridges, and the Kleene connectives ----------
// A boolean Value carries the three-valued domain directly: TRUE = boolValue(true),
// FALSE = boolValue(false), UNKNOWN = null. The comparison primitives (eq3/lt3/gt3)
// speak ThreeValued; from3 lifts their result into a boolean Value, and to3 projects a
// Value back so the AND/OR/NOT connectives operate on one domain.

// from3 lifts a three-valued result into a boolean Value (UNKNOWN → NULL).
export function from3(t: ThreeValued): Value {
  if (t === "true") return boolValue(true);
  if (t === "false") return boolValue(false);
  return nullValue();
}

// to3 projects a Value into the three-valued domain. A non-boolean Value is UNKNOWN.
export function to3(v: Value): ThreeValued {
  if (v.kind !== "bool") return "unknown";
  return v.value ? "true" : "false";
}

// boolAnd is Kleene AND: FALSE dominates (false AND unknown = false); TRUE only when
// both are TRUE; otherwise UNKNOWN (NULL). This is why AND is not plain propagation.
export function boolAnd(a: Value, b: Value): Value {
  const ta = to3(a);
  const tb = to3(b);
  if (ta === "false" || tb === "false") return boolValue(false);
  if (ta === "true" && tb === "true") return boolValue(true);
  return nullValue();
}

// boolOr is Kleene OR: TRUE dominates (true OR unknown = true).
export function boolOr(a: Value, b: Value): Value {
  const ta = to3(a);
  const tb = to3(b);
  if (ta === "true" || tb === "true") return boolValue(true);
  if (ta === "false" && tb === "false") return boolValue(false);
  return nullValue();
}

// boolNot is Kleene NOT: genuine propagation — NOT NULL = NULL.
export function boolNot(a: Value): Value {
  const t = to3(a);
  if (t === "true") return boolValue(false);
  if (t === "false") return boolValue(true);
  return nullValue();
}
