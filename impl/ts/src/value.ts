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
  | { kind: "bool"; value: boolean };

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
    default:
      return v.int.toString();
  }
}

// eq3 is three-valued equality. NULL compared with anything (including NULL) is
// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Since all integer
// types promote losslessly into the common bigint, cross-type comparison is just
// bigint equality (spec/types/compare.toml).
export function eq3(a: Value, b: Value): ThreeValued {
  if (a.kind !== "int" || b.kind !== "int") return "unknown";
  return bool3(a.int === b.int);
}

// lt3 is the three-valued ordering predicate a < b.
export function lt3(a: Value, b: Value): ThreeValued {
  if (a.kind !== "int" || b.kind !== "int") return "unknown";
  return bool3(a.int < b.int);
}

// gt3 is the three-valued ordering predicate a > b.
export function gt3(a: Value, b: Value): ThreeValued {
  if (a.kind !== "int" || b.kind !== "int") return "unknown";
  return bool3(a.int > b.int);
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
