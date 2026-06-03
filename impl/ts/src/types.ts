// Scalar type system (CLAUDE.md §4). Step-1 scope: signed integers only. Hand-written
// per CLAUDE.md §5, cross-checked against spec/types/scalars.toml in tests so the two
// never drift.
//
// ScalarType is a string-literal union (not a TS enum — the elidable subset forbids
// enums); the member IS the canonical name. All bounds are `bigint` (CLAUDE.md: int64
// exceeds JS's safe-integer range, so every integer flows through bigint — uniform,
// exact at all widths).

export type ScalarType = "int16" | "int32" | "int64" | "text";

export const ALL_SCALAR_TYPES: readonly ScalarType[] = ["int16", "int32", "int64", "text"];

// isText reports whether this is the variable-width text type (vs a fixed-width integer).
// text has collation C (UTF-8 byte / code-point order — spec/design/types.md §11). The
// integer-only helpers below (widthBytes/minOf/maxOf/rank) throw on "text"; callers route
// text through its own paths (the value codec, the text comparator), never these.
export function isText(t: ScalarType): boolean {
  return t === "text";
}

// canonicalName is the single name used in all output (determinism — CLAUDE.md §10).
// It is the identity for this union, but kept as a function to mirror the Go/Rust API.
export function canonicalName(t: ScalarType): string {
  return t;
}

// scalarTypeFromName resolves a type name (canonical or alias) case-insensitively, or
// undefined. PG's int2/int4/int8 are intentionally NOT accepted (we own our surface §1).
export function scalarTypeFromName(name: string): ScalarType | undefined {
  switch (name.toLowerCase()) {
    case "int16":
    case "smallint":
      return "int16";
    case "int32":
    case "int":
    case "integer":
      return "int32";
    case "int64":
    case "bigint":
      return "int64";
    // The two-word "character varying" alias is recognized, though this slice's parser
    // only produces single-word type names (a documented narrowing — types.md §11).
    case "text":
    case "varchar":
    case "string":
    case "character varying":
      return "text";
    default:
      return undefined;
  }
}

// widthBytes is the fixed storage width in bytes (the key-encoding width). Integer-only —
// text is variable-width and carries its own length (spec/fileformat/format.md), so this
// throws on "text" and is never used on that path.
export function widthBytes(t: ScalarType): number {
  switch (t) {
    case "int16":
      return 2;
    case "int32":
      return 4;
    case "int64":
      return 8;
    case "text":
      throw new Error("text is variable-width; widthBytes is integer-only");
  }
}

// minOf is the inclusive minimum value (integer-only).
export function minOf(t: ScalarType): bigint {
  switch (t) {
    case "int16":
      return -32768n;
    case "int32":
      return -2147483648n;
    case "int64":
      return -9223372036854775808n;
    case "text":
      throw new Error("text has no integer range");
  }
}

// maxOf is the inclusive maximum value (integer-only).
export function maxOf(t: ScalarType): bigint {
  switch (t) {
    case "int16":
      return 32767n;
    case "int32":
      return 2147483647n;
    case "int64":
      return 9223372036854775807n;
    case "text":
      throw new Error("text has no integer range");
  }
}

// rank is the promotion-tower rank: int16 < int32 < int64 (spec/types/compare.toml).
// Integer-only — text does not promote (there is one text type).
export function rank(t: ScalarType): number {
  switch (t) {
    case "int16":
      return 1;
    case "int32":
      return 2;
    case "int64":
      return 3;
    case "text":
      throw new Error("text has no promotion rank");
  }
}

// inRange reports whether v fits this type's inclusive range.
export function inRange(t: ScalarType, v: bigint): boolean {
  return v >= minOf(t) && v <= maxOf(t);
}

// isBooleanTypeName reports whether name is the boolean type (canonical "boolean",
// alias "bool"), case-insensitively. boolean is a known scalar (spec/types/scalars.toml,
// storable = false) that exists only as an expression type this slice — it is not a
// ScalarType because it cannot be a column or CAST target. Used to distinguish a
// known-but-not-storable type name (→ 0A000) from a genuinely unknown one (→ 42704).
export function isBooleanTypeName(name: string): boolean {
  const lower = name.toLowerCase();
  return lower === "boolean" || lower === "bool";
}
