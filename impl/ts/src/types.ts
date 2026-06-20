// Scalar type system (CLAUDE.md §4). Step-1 scope: signed integers only. Hand-written
// per CLAUDE.md §5, cross-checked against spec/types/scalars.toml in tests so the two
// never drift.
//
// ScalarType is a string-literal union (not a TS enum — the elidable subset forbids
// enums); the member IS the canonical name. All bounds are `bigint` (CLAUDE.md: i64
// exceeds JS's safe-integer range, so every integer flows through bigint — uniform,
// exact at all widths).

import { RANGES } from "./ranges_gen.ts";

export type ScalarType =
  | "i16"
  | "i32"
  | "i64"
  | "f32"
  | "f64"
  | "text"
  | "boolean"
  | "decimal"
  | "bytea"
  | "uuid"
  | "timestamp"
  | "timestamptz"
  | "interval"
  | "date";

export const ALL_SCALAR_TYPES: readonly ScalarType[] = [
  "i16",
  "i32",
  "i64",
  "f32",
  "f64",
  "text",
  "boolean",
  "decimal",
  "bytea",
  "uuid",
  "timestamp",
  "timestamptz",
  "interval",
  "date",
];

// DecimalTypmod is a decimal column's numeric(precision, scale) type modifier. precision >= 1;
// an unconstrained numeric column carries no typmod (spec/design/decimal.md §2). Validated at
// resolve (1 <= precision <= 1000, 0 <= scale <= precision; else 22023).
export type DecimalTypmod = { precision: number; scale: number };

// isText reports whether this is the variable-width text type (vs a fixed-width integer).
// text has collation C (UTF-8 byte / code-point order — spec/design/types.md §11). The
// integer-only helpers below (widthBytes/minOf/maxOf/rank) throw on "text"/"boolean"/"decimal";
// callers route those through their own paths (the value codec, the comparators), never these.
export function isText(t: ScalarType): boolean {
  return t === "text";
}

// isBool reports whether this is the boolean type (false/true; stored as a bool-byte —
// spec/design/types.md §9).
export function isBool(t: ScalarType): boolean {
  return t === "boolean";
}

// isDecimal reports whether this is the exact decimal type.
export function isDecimal(t: ScalarType): boolean {
  return t === "decimal";
}

// isBytea reports whether this is the variable-width bytea type (raw bytes), compared by
// unsigned byte order — spec/design/types.md §13.
export function isBytea(t: ScalarType): boolean {
  return t === "bytea";
}

// isUuid reports whether this is the fixed 16-byte uuid type (compared by unsigned byte
// order — spec/design/types.md §14). The first non-integer type usable as a key.
export function isUuid(t: ScalarType): boolean {
  return t === "uuid";
}

// isTimestamp reports whether this is the zoneless timestamp type (spec/design/timestamp.md).
export function isTimestamp(t: ScalarType): boolean {
  return t === "timestamp";
}

// isTimestamptz reports whether this is the UTC-instant timestamptz type.
export function isTimestamptz(t: ScalarType): boolean {
  return t === "timestamptz";
}

// isInterval reports whether this is the interval (span) type.
export function isInterval(t: ScalarType): boolean {
  return t === "interval";
}

// isDate reports whether this is the date (calendar date) type (spec/design/date.md).
export function isDate(t: ScalarType): boolean {
  return t === "date";
}

// isInteger reports whether this is one of the fixed-width signed integer types.
export function isInteger(t: ScalarType): boolean {
  return t === "i16" || t === "i32" || t === "i64";
}

// isFloat reports whether this is one of the two IEEE 754 binary float types
// (spec/design/float.md). f32 = binary32 (single), f64 = binary64 (double).
export function isFloat(t: ScalarType): boolean {
  return t === "f32" || t === "f64";
}

// canonicalName is the single name used in all output (determinism — CLAUDE.md §10).
// It is the identity for this union, but kept as a function to mirror the Go/Rust API.
export function canonicalName(t: ScalarType): string {
  return t;
}

// scalarTypeFromName resolves a type name (canonical or alias) case-insensitively, or
// undefined. Canonical names state width in bits under the i/f prefix (i16/i32/i64, f32/f64
// — the Rust/Zig convention). Accepted aliases: the SQL-standard words (smallint/int/integer/
// bigint, real/double precision/float) AND PG's byte-shorthand (int2/int4/int8, float4/float8).
// The byte-shorthand is safe to accept BECAUSE of the i/f prefix: jed's bit-namespace (i8…i64)
// is lexically disjoint from PG's byte-namespace (int2…int8), so int8 → i64 with no collision
// and a future 8-bit i8 stays free (spec/design/types.md §11; CLAUDE.md §1/§4).
export function scalarTypeFromName(name: string): ScalarType | undefined {
  switch (name.toLowerCase()) {
    case "i16":
    case "smallint":
    case "int2":
      return "i16";
    case "i32":
    case "int":
    case "integer":
    case "int4":
      return "i32";
    case "i64":
    case "bigint":
    case "int8":
      return "i64";
    // Float types (spec/design/float.md §2). The promotion tower's canonical ids state width
    // in bits (f32/f64); the SQL-standard names (real, double precision, float) and PG's
    // byte-shorthand (float4/float8) are aliases. Note the C/Java-counterintuitive PG rule:
    // a bare `float` (no precision) IS double precision (so it maps to f64, not f32).
    case "f32":
    case "real":
    case "float4":
      return "f32";
    case "f64":
    case "double precision":
    case "float":
    case "float8":
      return "f64";
    // The two-word "character varying" alias is recognized, though this slice's parser
    // only produces single-word type names (a documented narrowing — types.md §11).
    case "text":
    case "varchar":
    case "string":
    case "character varying":
      return "text";
    case "boolean":
    case "bool":
      return "boolean";
    case "decimal":
    case "numeric":
    case "dec":
      return "decimal";
    case "bytea":
      return "bytea";
    case "uuid":
      return "uuid";
    case "timestamp":
    case "timestamp without time zone":
      return "timestamp";
    case "timestamptz":
    case "timestamp with time zone":
      return "timestamptz";
    case "interval":
      return "interval";
    case "date":
      return "date";
    default:
      return undefined;
  }
}

// widthBytes is the fixed storage width in bytes (the KEY-encoding width — the bare key body,
// no presence tag — for the fixed-width keyable types: the three integers, the two i64
// timestamps, uuid (16), and boolean (1 — the bool-byte key, spec/design/encoding.md §2.9)).
// Used by the index tail-slot skip (each self-delimiting component is 0x01 NULL or 0x00 + this
// many bytes). text/decimal/bytea/interval are variable-width / non-key and throw — they carry
// their own length (spec/fileformat/format.md). uuid is the first non-integer fixed-width key
// type; callers branch on isUuid before the integer decode path, since decodeInt would sign-flip
// its bytes. (boolean's VALUE codec has its own 1-byte branch and never reaches here; this width
// is the key path only.)
export function widthBytes(t: ScalarType): number {
  switch (t) {
    case "boolean":
      return 1;
    case "i16":
      return 2;
    case "i32":
      return 4;
    case "i64":
    // The two timestamps are i64-microsecond instants — fixed-width 8-byte, reusing the
    // i64 key/value codec (spec/design/timestamp.md §6).
    case "timestamp":
    case "timestamptz":
      return 8;
    case "uuid":
      return 16;
    // The two IEEE binary floats are fixed-width like the integers: 4 bytes (binary32) and
    // 8 bytes (binary64). They use the float value codec (DataView, big-endian) on disk and
    // the float-order-preserving key encoding — both keyed on this width (spec/design/float.md
    // §10). They are NOT routed through the integer key/value codec.
    case "f32":
      return 4;
    case "f64":
      return 8;
    case "text":
      throw new Error("text is variable-width; widthBytes is integer-only");
    case "decimal":
      throw new Error("decimal is variable-width; widthBytes is integer-only");
    case "bytea":
      throw new Error("bytea is variable-width; widthBytes is integer-only");
    case "interval":
      throw new Error("interval is not serialized through the integer codec; widthBytes is integer-only");
    // A date is a fixed-width 4-byte i32 day count (reuses the i32 codec — it is a key this
    // slice, like timestamp; spec/design/date.md).
    case "date":
      return 4;
  }
}

// minOf is the inclusive minimum value (integer-only).
export function minOf(t: ScalarType): bigint {
  switch (t) {
    case "i16":
      return -32768n;
    case "i32":
      return -2147483648n;
    case "i64":
      return -9223372036854775808n;
    case "f32":
    case "f64":
      throw new Error("float has no integer range");
    case "text":
      throw new Error("text has no integer range");
    case "boolean":
      throw new Error("boolean has no integer range");
    case "decimal":
      throw new Error("decimal has no integer range");
    case "bytea":
      throw new Error("bytea has no integer range");
    case "uuid":
      throw new Error("uuid has no integer range");
    case "timestamp":
    case "timestamptz":
      throw new Error("timestamp has no integer range");
    case "interval":
      throw new Error("interval has no integer range");
    case "date":
      throw new Error("date has no integer range");
  }
}

// maxOf is the inclusive maximum value (integer-only).
export function maxOf(t: ScalarType): bigint {
  switch (t) {
    case "i16":
      return 32767n;
    case "i32":
      return 2147483647n;
    case "i64":
      return 9223372036854775807n;
    case "f32":
    case "f64":
      throw new Error("float has no integer range");
    case "text":
      throw new Error("text has no integer range");
    case "boolean":
      throw new Error("boolean has no integer range");
    case "decimal":
      throw new Error("decimal has no integer range");
    case "bytea":
      throw new Error("bytea has no integer range");
    case "uuid":
      throw new Error("uuid has no integer range");
    case "timestamp":
    case "timestamptz":
      throw new Error("timestamp has no integer range");
    case "interval":
      throw new Error("interval has no integer range");
    case "date":
      throw new Error("date has no integer range");
  }
}

// rank is the promotion-tower rank: i16 < i32 < i64 (spec/types/compare.toml).
// Integer-only — text does not promote (there is one text type).
export function rank(t: ScalarType): number {
  switch (t) {
    case "i16":
      return 1;
    case "i32":
      return 2;
    case "i64":
      return 3;
    case "f32":
    case "f64":
      throw new Error("float uses floatRank, not the integer promotion rank");
    case "text":
      throw new Error("text has no promotion rank");
    case "boolean":
      throw new Error("boolean has no promotion rank");
    case "decimal":
      throw new Error("decimal has no integer promotion rank");
    case "bytea":
      throw new Error("bytea has no promotion rank");
    case "uuid":
      throw new Error("uuid has no promotion rank");
    case "timestamp":
    case "timestamptz":
      throw new Error("timestamp has no promotion rank");
    case "interval":
      throw new Error("interval has no promotion rank");
    case "date":
      throw new Error("date has no promotion rank");
  }
}

// inRange reports whether v fits this type's inclusive range.
export function inRange(t: ScalarType, v: bigint): boolean {
  return v >= minOf(t) && v <= maxOf(t);
}

// floatRank is the float-family promotion-tower rank: f32 (1) < f64 (2)
// (spec/types/compare.toml `max-rank`; spec/design/float.md §2). When two floats of different
// width meet (arithmetic / comparison) both widen to the higher rank — f32 → f64,
// which is lossless. Separate from the integer `rank`: the two towers never mix (cross-family
// int↔float is an explicit cast, never an implicit promotion). float-only; throws otherwise.
export function floatRank(t: ScalarType): number {
  if (t === "f32") return 1;
  if (t === "f64") return 2;
  throw new Error("floatRank is float-only");
}

// promoteFloat is the higher-rank of two float types (the float tower; float.md §2). Used to
// settle the result type of a mixed-width float arithmetic / comparison node.
export function promoteFloat(a: ScalarType, b: ScalarType): ScalarType {
  return floatRank(a) >= floatRank(b) ? a : b;
}

// roundToWidth rounds a JS number to the exact value representable at this float type's width:
// identity for f64 (JS `number` IS binary64) and `Math.fround` for f32 (true binary32
// rounding). It MUST be applied on EVERY f32 operation, literal, cast, and result — JS does
// all arithmetic in binary64, so without it a f32 value would carry binary64 precision and
// diverge from the Rust/Go cores (spec/design/float.md §2, the one extra TS discipline).
export function roundToWidth(ty: ScalarType, v: number): number {
  return ty === "f32" ? Math.fround(v) : v;
}

// Type is a column / value type: either a built-in ScalarType or a by-name reference to a
// user-defined COMPOSITE (row) type (spec/design/composite.md). This is the *open* wrapper above
// the closed ScalarType union (CLAUDE.md §4): the scalar set stays a fixed compiled-in union, but
// a column type can now also name a composite living in the database's type catalog. Modeled as a
// discriminated union (keyed on `kind`, like Value), with free-function helpers below to match the
// boring/explicit style (CLAUDE.md §10) — never methods on the union. As of slice S1 no composite
// can yet be created; scalar-only paths call typeScalar(t).
export type Type =
  | { kind: "scalar"; scalar: ScalarType }
  | { kind: "composite"; name: string }
  | { kind: "array"; elem: Type }
  | { kind: "range"; elem: Type };

// scalarT wraps a ScalarType as a Type.
export function scalarT(s: ScalarType): Type {
  return { kind: "scalar", scalar: s };
}

// compositeT makes a by-name reference to a composite type in the database's type catalog. The
// display name is case-preserved; lookups lowercase it (the table-name convention).
export function compositeT(name: string): Type {
  return { kind: "composite", name };
}

// arrayT builds a structural array Type over an element type (spec/design/array.md §2). The element
// type is carried inline (no catalog object, unlike a composite); the element is a scalar or
// composite, never another array (multidimensionality is a value property, not array-of-array).
export function arrayT(elem: Type): Type {
  return { kind: "array", elem };
}

// isArrayType reports whether this is an array type.
export function isArrayType(t: Type): boolean {
  return t.kind === "array";
}

// rangeT builds a structural range Type over a scalar element/subtype (spec/design/ranges.md §2).
export function rangeT(elem: Type): Type {
  return { kind: "range", elem };
}

// isRangeType reports whether this is a range type.
export function isRangeType(t: Type): boolean {
  return t.kind === "range";
}

// compositeRefName returns the composite type this type references, looking through one array level —
// the name for both `addr` and `addr[]`, null for a scalar or a `scalar[]`. There is at most one
// (arrays are over a single element; composites are referenced by name, never inlined), so the
// dependency-tracking (DROP TYPE) and two-pass-load validation paths use this to find a composite
// reference whether it is direct or wrapped in an array field/column (spec/design/array.md §12).
export function compositeRefName(t: Type): string | null {
  if (t.kind === "composite") return t.name;
  if (t.kind === "array") return compositeRefName(t.elem);
  return null;
}

// typeScalar returns the inner scalar type. Scalar-only paths (the integer codec, the scalar value
// codec, the scalar resolver) call this; a composite column reaches those paths only after the
// caller has branched on isCompositeType, so a composite here is an engine-invariant violation —
// it throws (mirroring Rust's unreachable!). In S1 no composite Type exists yet.
export function typeScalar(t: Type): ScalarType {
  if (t.kind === "scalar") return t.scalar;
  if (t.kind === "array") {
    throw new Error(
      "array type used where a scalar was expected (spec/design/array.md)",
    );
  }
  if (t.kind === "range") {
    throw new Error(
      "range type used where a scalar was expected (spec/design/ranges.md)",
    );
  }
  throw new Error(
    `composite type ${t.name} used where a scalar was expected; the composite path must branch before this point (spec/design/composite.md)`,
  );
}

// typeAsScalar returns the inner scalar type, or undefined for a composite/array.
export function typeAsScalar(t: Type): ScalarType | undefined {
  return t.kind === "scalar" ? t.scalar : undefined;
}

// isCompositeType reports whether this is a composite (user-defined row) type.
export function isCompositeType(t: Type): boolean {
  return t.kind === "composite";
}

// typeCanonicalName is this type's canonical name for output / error messages — the scalar's
// canonical name, the composite's name, or `<elem>[]` for an array.
export function typeCanonicalName(t: Type): string {
  if (t.kind === "scalar") return canonicalName(t.scalar);
  if (t.kind === "array") return typeCanonicalName(t.elem) + "[]";
  if (t.kind === "range") {
    // A range's canonical name comes from ranges.toml keyed by the element (i32 → i32range). The
    // RANGES table is pure data (no import cycle), so the lookup lives here directly.
    if (t.elem.kind === "scalar") {
      const ename = canonicalName(t.elem.scalar);
      const name = RANGES.find((r) => r.element === ename)?.id;
      if (name !== undefined) return name;
    }
    return `range<${typeCanonicalName(t.elem)}>`;
  }
  return t.name;
}

// Scalar-predicate delegates. A composite answers false to every scalar predicate — it is none of
// these families — so keyability checks (isInteger || isUuid || …) correctly reject a composite
// (0A000), and family branches fall through to their composite handling.
export function typeIsInteger(t: Type): boolean {
  return t.kind === "scalar" && isInteger(t.scalar);
}
export function typeIsDecimal(t: Type): boolean {
  return t.kind === "scalar" && isDecimal(t.scalar);
}
export function typeIsBoolean(t: Type): boolean {
  return t.kind === "scalar" && isBool(t.scalar);
}
export function typeIsText(t: Type): boolean {
  return t.kind === "scalar" && isText(t.scalar);
}
export function typeIsBytea(t: Type): boolean {
  return t.kind === "scalar" && isBytea(t.scalar);
}
export function typeIsUuid(t: Type): boolean {
  return t.kind === "scalar" && isUuid(t.scalar);
}
export function typeIsTimestamp(t: Type): boolean {
  return t.kind === "scalar" && isTimestamp(t.scalar);
}
export function typeIsTimestamptz(t: Type): boolean {
  return t.kind === "scalar" && isTimestamptz(t.scalar);
}
export function typeIsDate(t: Type): boolean {
  return t.kind === "scalar" && isDate(t.scalar);
}
