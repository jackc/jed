// Runtime values and three-valued (Kleene) logic.
//
// A Value is SQL NULL or an integer. All step-1 scalar types are signed integers; a
// non-null value is held as a `bigint` regardless of its declared column type (so
// int64 is exact — JS `number` cannot represent the full int64 range). The declared
// type governs range checks and key-encoding width, not the in-memory representation.

export type Value = { kind: "null" } | { kind: "int"; int: bigint };

// intValue builds a non-null integer value.
export function intValue(n: bigint): Value {
  return { kind: "int", int: n };
}

// nullValue builds a NULL value.
export function nullValue(): Value {
  return { kind: "null" };
}

// ThreeValued is the result of a three-valued comparison (CLAUDE.md §4):
// TRUE / FALSE / UNKNOWN. UNKNOWN arises whenever a NULL participates.
export type ThreeValued = "true" | "false" | "unknown";

// isTrue reports whether a WHERE predicate selects a row: only TRUE selects; UNKNOWN
// (NULL) and FALSE both reject (CLAUDE.md §4).
export function isTrue(t: ThreeValued): boolean {
  return t === "true";
}

function bool3(b: boolean): ThreeValued {
  return b ? "true" : "false";
}

// render formats for conformance output: integers as shortest decimal, NULL as the
// literal "NULL" (spec/design/conformance.md §1). BigInt#toString gives the plain
// decimal with no `n` suffix, matching Go's strconv.FormatInt.
export function render(v: Value): string {
  return v.kind === "null" ? "NULL" : v.int.toString();
}

// eq3 is three-valued equality. NULL compared with anything (including NULL) is
// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Since all integer
// types promote losslessly into the common bigint, cross-type comparison is just
// bigint equality (spec/types/compare.toml).
export function eq3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  return bool3(a.int === b.int);
}

// lt3 is the three-valued ordering predicate a < b.
export function lt3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  return bool3(a.int < b.int);
}

// gt3 is the three-valued ordering predicate a > b.
export function gt3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  return bool3(a.int > b.int);
}
