// Scalar type system (CLAUDE.md §4). Step-1 scope: signed integers only. Hand-written
// per CLAUDE.md §5, cross-checked against spec/types/scalars.toml in tests so the two
// never drift.
//
// ScalarType is a string-literal union (not a TS enum — the elidable subset forbids
// enums); the member IS the canonical name. All bounds are `bigint` (CLAUDE.md: int64
// exceeds JS's safe-integer range, so every integer flows through bigint — uniform,
// exact at all widths).

export type ScalarType = "int16" | "int32" | "int64" | "text" | "boolean" | "decimal" | "bytea";

export const ALL_SCALAR_TYPES: readonly ScalarType[] = [
  "int16",
  "int32",
  "int64",
  "text",
  "boolean",
  "decimal",
  "bytea",
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

// isInteger reports whether this is one of the fixed-width signed integer types.
export function isInteger(t: ScalarType): boolean {
  return t === "int16" || t === "int32" || t === "int64";
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
    case "boolean":
    case "bool":
      return "boolean";
    case "decimal":
    case "numeric":
    case "dec":
      return "decimal";
    case "bytea":
      return "bytea";
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
    case "boolean":
      throw new Error("boolean uses the bool-byte codec; widthBytes is integer-only");
    case "decimal":
      throw new Error("decimal is variable-width; widthBytes is integer-only");
    case "bytea":
      throw new Error("bytea is variable-width; widthBytes is integer-only");
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
    case "boolean":
      throw new Error("boolean has no integer range");
    case "decimal":
      throw new Error("decimal has no integer range");
    case "bytea":
      throw new Error("bytea has no integer range");
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
    case "boolean":
      throw new Error("boolean has no integer range");
    case "decimal":
      throw new Error("decimal has no integer range");
    case "bytea":
      throw new Error("bytea has no integer range");
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
    case "boolean":
      throw new Error("boolean has no promotion rank");
    case "decimal":
      throw new Error("decimal has no integer promotion rank");
    case "bytea":
      throw new Error("bytea has no promotion rank");
  }
}

// inRange reports whether v fits this type's inclusive range.
export function inRange(t: ScalarType, v: bigint): boolean {
  return v >= minOf(t) && v <= maxOf(t);
}
