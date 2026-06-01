// Scalar type system (CLAUDE.md §4). Step-1 scope: signed integers only. Hand-written
// per CLAUDE.md §5, cross-checked against spec/types/scalars.toml in tests so the two
// never drift.
//
// ScalarType is a string-literal union (not a TS enum — the elidable subset forbids
// enums); the member IS the canonical name. All bounds are `bigint` (CLAUDE.md: int64
// exceeds JS's safe-integer range, so every integer flows through bigint — uniform,
// exact at all widths).

export type ScalarType = "int16" | "int32" | "int64";

export const ALL_SCALAR_TYPES: readonly ScalarType[] = ["int16", "int32", "int64"];

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
    default:
      return undefined;
  }
}

// widthBytes is the storage width in bytes (the key-encoding width).
export function widthBytes(t: ScalarType): number {
  switch (t) {
    case "int16":
      return 2;
    case "int32":
      return 4;
    case "int64":
      return 8;
  }
}

// minOf is the inclusive minimum value.
export function minOf(t: ScalarType): bigint {
  switch (t) {
    case "int16":
      return -32768n;
    case "int32":
      return -2147483648n;
    case "int64":
      return -9223372036854775808n;
  }
}

// maxOf is the inclusive maximum value.
export function maxOf(t: ScalarType): bigint {
  switch (t) {
    case "int16":
      return 32767n;
    case "int32":
      return 2147483647n;
    case "int64":
      return 9223372036854775807n;
  }
}

// rank is the promotion-tower rank: int16 < int32 < int64 (spec/types/compare.toml).
export function rank(t: ScalarType): number {
  switch (t) {
    case "int16":
      return 1;
    case "int32":
      return 2;
    case "int64":
      return 3;
  }
}

// inRange reports whether v fits this type's inclusive range.
export function inRange(t: ScalarType, v: bigint): boolean {
  return v >= minOf(t) && v <= maxOf(t);
}
