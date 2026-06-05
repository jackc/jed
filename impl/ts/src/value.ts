// Runtime values and three-valued (Kleene) logic.
//
// A Value is SQL NULL, an integer, a boolean, or text. Integers are held as `bigint`
// regardless of declared column type (so int64 is exact — JS `number` cannot represent
// the full int64 range); the declared type governs range checks and key-encoding width.
// A "bool" Value is produced by comparisons and connectives, can be projected/rendered,
// and — now that boolean is storable (spec/design/types.md §9) — is stored in a boolean
// column; a NULL boolean (unknown) is the "null" Value, so {true, false, NULL} is the
// three-valued domain, ordered false < true.

import { Decimal } from "./decimal.ts";
import { renderTimestamp, renderTimestamptz } from "./timestamp.ts";

export type Value =
  | { kind: "null" }
  | { kind: "int"; int: bigint }
  | { kind: "bool"; value: boolean }
  // A zoneless timestamp / UTC-instant timestamptz; micros is the int64 microsecond instant
  // (held as `bigint`, never `number` — the sentinels NEG_INFINITY/POS_INFINITY are
  // -infinity/+infinity). They compare by the instant and never cross-family (timestamp.ts).
  | { kind: "timestamp"; micros: bigint }
  | { kind: "timestamptz"; micros: bigint }
  // The first stored non-integer value; compares by the C collation (UTF-8 byte /
  // code-point order — spec/design/types.md §11). NOT compared with JS `<`/localeCompare,
  // which use UTF-16 code-unit order and disagree above U+FFFF (see compareTextC below).
  | { kind: "text"; text: string }
  // An exact base-10 decimal (spec/design/decimal.md). Value-equality is scale-insensitive
  // (1.5 == 1.50) and goes through eq3/cmpValue / the DISTINCT value-canonical key.
  | { kind: "decimal"; dec: Decimal }
  // A raw byte string (the bytea column type); compares by UNSIGNED byte order (§13). It
  // holds a Uint8Array — already raw bytes, so unlike text there is NO UTF-16 trap here.
  | { kind: "bytea"; bytes: Uint8Array }
  // A fixed 16-byte UUID (RFC 4122); compares by UNSIGNED byte order over the 16 bytes (§14).
  // Holds a 16-byte Uint8Array — a distinct kind from bytea (renders 8-4-4-4-12, its own
  // comparison family), so a uuid never equals a bytea even with identical bytes.
  | { kind: "uuid"; bytes: Uint8Array };

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

// decimalValue builds a non-null decimal value.
export function decimalValue(d: Decimal): Value {
  return { kind: "decimal", dec: d };
}

// byteaValue builds a non-null bytea value from raw bytes.
export function byteaValue(b: Uint8Array): Value {
  return { kind: "bytea", bytes: b };
}

// uuidValue builds a non-null uuid value from its 16 raw bytes (parseUuid guarantees 16).
export function uuidValue(b: Uint8Array): Value {
  return { kind: "uuid", bytes: b };
}

// timestampValue builds a non-null timestamp from its int64 microsecond instant.
export function timestampValue(m: bigint): Value {
  return { kind: "timestamp", micros: m };
}

// timestamptzValue builds a non-null timestamptz from its int64 microsecond instant.
export function timestamptzValue(m: bigint): Value {
  return { kind: "timestamptz", micros: m };
}

// compareBytea compares two byte strings by UNSIGNED byte order (Uint8Array elements are
// 0–255, so a direct element comparison is unsigned). Returns <0, 0, >0. No encoding step,
// so the UTF-16 trap that complicates text (compareTextC) does not apply. A shorter value
// that is a prefix of a longer one sorts first.
export function compareBytea(a: Uint8Array, b: Uint8Array): number {
  const n = a.length < b.length ? a.length : b.length;
  for (let i = 0; i < n; i++) {
    if (a[i] !== b[i]) return a[i]! < b[i]! ? -1 : 1;
  }
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
}

const HEX_DIGITS = "0123456789abcdef";

// renderByteaHex renders raw bytes as PostgreSQL's hex output form: a `\x` prefix followed
// by the LOWERCASE hex of each byte. The empty value renders as the bare prefix `\x`. The
// spelling must be byte-identical across cores (CLAUDE.md §8).
export function renderByteaHex(bytes: Uint8Array): string {
  let s = "\\x";
  for (let i = 0; i < bytes.length; i++) {
    const byte = bytes[i]!;
    s += HEX_DIGITS[byte >> 4]! + HEX_DIGITS[byte & 0xf]!;
  }
  return s;
}

// parseByteaHex decodes a bytea literal from its hex input form (spec/design/types.md §13):
// a `\x` prefix followed by an even count of hexadecimal digits (case-insensitive), each
// pair one byte; `\x` alone is the empty byte string. The inverse of renderByteaHex, so a
// value round-trips. The traditional escape input format is not accepted (a documented
// narrowing). Returns the bytes on success, or { error } describing malformed input (the
// caller raises it as a 22P02).
export function parseByteaHex(s: string): { bytes: Uint8Array } | { error: string } {
  if (s.length < 2 || s[0] !== "\\" || s[1] !== "x") {
    return { error: "bytea hex input must begin with \\x" };
  }
  const digits = s.slice(2);
  if (digits.length % 2 !== 0) {
    return { error: "bytea hex input has an odd number of digits" };
  }
  const out = new Uint8Array(digits.length / 2);
  for (let i = 0; i < digits.length; i += 2) {
    const hi = hexVal(digits.charCodeAt(i));
    const lo = hexVal(digits.charCodeAt(i + 1));
    if (hi < 0 || lo < 0) {
      return { error: "invalid hexadecimal digit in bytea input" };
    }
    out[i / 2] = (hi << 4) | lo;
  }
  return { bytes: out };
}

// hexVal returns one hex digit's value (0–15) from its char code, or -1 if not [0-9a-fA-F].
function hexVal(c: number): number {
  if (c >= 48 && c <= 57) return c - 48; // 0-9
  if (c >= 97 && c <= 102) return c - 97 + 10; // a-f
  if (c >= 65 && c <= 70) return c - 65 + 10; // A-F
  return -1;
}

// renderUuid formats 16 bytes as the canonical RFC 4122 text form: 32 LOWERCASE hex digits in
// the 8-4-4-4-12 grouping joined by hyphens (PostgreSQL uuid_out). Byte-identical across cores
// (CLAUDE.md §8), so the case and grouping are fixed here.
export function renderUuid(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < 16; i++) {
    if (i === 4 || i === 6 || i === 8 || i === 10) s += "-";
    const byte = bytes[i]!;
    s += HEX_DIGITS[byte >> 4]! + HEX_DIGITS[byte & 0xf]!;
  }
  return s;
}

// parseUuid decodes a uuid literal replicating PostgreSQL's uuid_in (spec/design/types.md §14):
// an optional surrounding `{ }`, then 16 bytes as two hex digits each (case-insensitive), with an
// optional hyphen consumed only after a whole pair of bytes (odd byte index, never the last) — so
// the canonical 8-4-4-4-12 form, a hyphen-less 32-hex run, and the every-4-digit grouping all
// parse, while a hyphen elsewhere is rejected (PG's exact algorithm, not a looser strip-all). The
// inverse of renderUuid for the canonical form, so a value round-trips. Returns the bytes, or
// { error } describing malformed input (the caller raises it as a 22P02).
export function parseUuid(s: string): { bytes: Uint8Array } | { error: string } {
  let pos = 0;
  const braces = s.length > 0 && s[0] === "{";
  if (braces) pos = 1;
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i++) {
    if (pos + 1 >= s.length) return { error: "invalid uuid: too few hexadecimal digits" };
    const hi = hexVal(s.charCodeAt(pos));
    const lo = hexVal(s.charCodeAt(pos + 1));
    if (hi < 0 || lo < 0) return { error: "invalid hexadecimal digit in uuid" };
    out[i] = (hi << 4) | lo;
    pos += 2;
    // A hyphen is consumed only after a whole pair of bytes (odd byte index) and never after
    // the last byte — exactly PostgreSQL's string_to_uuid rule.
    if (i % 2 === 1 && i < 15 && s[pos] === "-") pos++;
  }
  if (braces) {
    if (s[pos] !== "}") return { error: "invalid uuid: missing or misplaced closing brace" };
    pos++;
  }
  if (pos !== s.length) return { error: "invalid uuid: trailing characters after the 16 bytes" };
  return { bytes: out };
}

// numericCmp compares two numeric values by value, promoting an integer operand to decimal
// when its sibling is decimal (the integer↔decimal cross-family rule — spec/types/compare.toml).
// Returns undefined for any non-numeric pair (text, boolean, NULL), which callers treat as
// UNKNOWN.
function numericCmp(a: Value, b: Value): number | undefined {
  if (a.kind === "int" && b.kind === "int") return a.int < b.int ? -1 : a.int > b.int ? 1 : 0;
  if (a.kind === "decimal" && b.kind === "decimal") return a.dec.cmpValue(b.dec);
  if (a.kind === "int" && b.kind === "decimal") return Decimal.fromBigInt(a.int).cmpValue(b.dec);
  if (a.kind === "decimal" && b.kind === "int") return a.dec.cmpValue(Decimal.fromBigInt(b.int));
  return undefined;
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
    case "decimal":
      // Decimal renders as its canonical base-10 string, preserving display scale
      // (the D tag — spec/design/decimal.md §6).
      return v.dec.render();
    case "bytea":
      return renderByteaHex(v.bytes);
    case "uuid":
      // Canonical 8-4-4-4-12 lowercase-hex form (PG uuid_out).
      return renderUuid(v.bytes);
    case "timestamp":
      return renderTimestamp(v.micros);
    case "timestamptz":
      return renderTimestamptz(v.micros);
    default:
      return v.int.toString();
  }
}

// eq3 is three-valued equality. NULL compared with anything (including NULL) is
// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers compare by
// value (all integer types promote losslessly into the common bigint); text by the C
// collation (UTF-8 byte order — string equality is exact for ===); booleans by value
// (false < true). A mixed cross-family pair never reaches here (the resolver rejects it,
// 42804); any other variant pair is a NULL.
export function eq3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  const c = numericCmp(a, b);
  if (c !== undefined) return bool3(c === 0);
  if (a.kind === "text" && b.kind === "text") return bool3(a.text === b.text);
  if (a.kind === "bytea" && b.kind === "bytea") return bool3(compareBytea(a.bytes, b.bytes) === 0);
  if (a.kind === "uuid" && b.kind === "uuid") return bool3(compareBytea(a.bytes, b.bytes) === 0);
  if (a.kind === "bool" && b.kind === "bool") return bool3(a.value === b.value);
  // Timestamps compare by the int64 instant (infinity is just an extreme value).
  if (a.kind === "timestamp" && b.kind === "timestamp") return bool3(a.micros === b.micros);
  if (a.kind === "timestamptz" && b.kind === "timestamptz") return bool3(a.micros === b.micros);
  return "unknown";
}

// lt3 is the three-valued ordering predicate a < b (numerics by value with int↔decimal
// promotion; text by C collation = UTF-8 byte order; boolean by value, false < true).
export function lt3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  const c = numericCmp(a, b);
  if (c !== undefined) return bool3(c < 0);
  if (a.kind === "text" && b.kind === "text") return bool3(compareTextC(a.text, b.text) < 0);
  if (a.kind === "bytea" && b.kind === "bytea") return bool3(compareBytea(a.bytes, b.bytes) < 0);
  if (a.kind === "uuid" && b.kind === "uuid") return bool3(compareBytea(a.bytes, b.bytes) < 0);
  if (a.kind === "bool" && b.kind === "bool") return bool3(!a.value && b.value);
  if (a.kind === "timestamp" && b.kind === "timestamp") return bool3(a.micros < b.micros);
  if (a.kind === "timestamptz" && b.kind === "timestamptz") return bool3(a.micros < b.micros);
  return "unknown";
}

// gt3 is the three-valued ordering predicate a > b (numerics by value with int↔decimal
// promotion; text by C collation = UTF-8 byte order; boolean by value, false < true).
export function gt3(a: Value, b: Value): ThreeValued {
  if (a.kind === "null" || b.kind === "null") return "unknown";
  const c = numericCmp(a, b);
  if (c !== undefined) return bool3(c > 0);
  if (a.kind === "text" && b.kind === "text") return bool3(compareTextC(a.text, b.text) > 0);
  if (a.kind === "bytea" && b.kind === "bytea") return bool3(compareBytea(a.bytes, b.bytes) > 0);
  if (a.kind === "uuid" && b.kind === "uuid") return bool3(compareBytea(a.bytes, b.bytes) > 0);
  if (a.kind === "bool" && b.kind === "bool") return bool3(a.value && !b.value);
  if (a.kind === "timestamp" && b.kind === "timestamp") return bool3(a.micros > b.micros);
  if (a.kind === "timestamptz" && b.kind === "timestamptz") return bool3(a.micros > b.micros);
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
