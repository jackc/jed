// Runtime values and three-valued (Kleene) logic.
//
// A Value is SQL NULL, an integer, a boolean, or text. Integers are held as `bigint`
// regardless of declared column type (so i64 is exact — JS `number` cannot represent
// the full i64 range); the declared type governs range checks and key-encoding width.
// A "bool" Value is produced by comparisons and connectives, can be projected/rendered,
// and — now that boolean is storable (spec/design/types.md §9) — is stored in a boolean
// column; a NULL boolean (unknown) is the "null" Value, so {true, false, NULL} is the
// three-valued domain, ordered false < true.

import { Decimal } from "./decimal.ts";
import { type Interval, intervalCmp, renderInterval } from "./interval.ts";
import { renderTimestamp, renderTimestamptz } from "./timestamp.ts";
import { renderDate } from "./date.ts";
import { rangeOut, rangeTotalCmp } from "./range.ts";
import { type JsonNode, jsonNodeCmp, jsonbOut } from "./json.ts";

export type Value =
  | { kind: "null" }
  | { kind: "int"; int: bigint }
  | { kind: "bool"; value: boolean }
  // A zoneless timestamp / UTC-instant timestamptz; micros is the i64 microsecond instant
  // (held as `bigint`, never `number` — the sentinels NEG_INFINITY/POS_INFINITY are
  // -infinity/+infinity). They compare by the instant and never cross-family (timestamp.ts).
  | { kind: "timestamp"; micros: bigint }
  | { kind: "timestamptz"; micros: bigint }
  // A calendar date; days is the i32 day count since 1970-01-01 (held as `bigint`, the core's
  // uniform-integer discipline; the sentinels DATE_NEG_INFINITY/DATE_POS_INFINITY are
  // -infinity/+infinity). Compares by the day count; renders YYYY-MM-DD (spec/design/date.md).
  | { kind: "date"; days: bigint }
  // An interval span — months/days/micros (spec/design/interval.md). Comparison/dedup go through
  // the canonical 128-bit span (intervalSpan), NOT field equality, so '1 mon' == '30 days' while
  // render preserves each value's fields. micros is a bigint (i64 exactness).
  | { kind: "interval"; iv: Interval }
  // The first stored non-integer value; compares by the C collation (UTF-8 byte /
  // code-point order — spec/design/types.md §11). NOT compared with JS `<`/localeCompare,
  // which use UTF-16 code-unit order and disagree above U+FFFF (see compareTextC below).
  | { kind: "text"; text: string }
  // An exact base-10 decimal (spec/design/decimal.md). Value-equality is scale-insensitive
  // (1.5 == 1.50) and goes through eq3/cmpValue / the DISTINCT value-canonical key.
  | { kind: "decimal"; dec: Decimal }
  // An IEEE 754 binary float (spec/design/float.md). `value` is a plain JS number — which IS
  // binary64, so a f64's number is exact; a f32's number is ALWAYS the `Math.fround`'d
  // value (true binary32), maintained by every constructor/op/cast/literal. NaN/±Infinity are
  // first-class. Comparison/dedup/keys use the TOTAL order (floatTotalCmp): -0 == +0, NaN == NaN,
  // NaN largest — NOT JS `<`/`===` (which give NaN!==NaN and -0===+0 wrong for ordering). Storage
  // preserves the bits verbatim (a stored -0.0 keeps its sign); canonicalization is a compare/key
  // concern only.
  | { kind: "f32"; value: number }
  | { kind: "f64"; value: number }
  // A raw byte string (the bytea column type); compares by UNSIGNED byte order (§13). It
  // holds a Uint8Array — already raw bytes, so unlike text there is NO UTF-16 trap here.
  | { kind: "bytea"; bytes: Uint8Array }
  // A fixed 16-byte UUID (RFC 4122); compares by UNSIGNED byte order over the 16 bytes (§14).
  // Holds a 16-byte Uint8Array — a distinct kind from bytea (renders 8-4-4-4-12, its own
  // comparison family), so a uuid never equals a bytea even with identical bytes.
  | { kind: "uuid"; bytes: Uint8Array }
  // A composite (row) value — an ordered list of field values, recursive (a field may itself be a
  // composite) — spec/design/composite.md §2. The field count and per-field types match the value's
  // composite type; the storage codec / comparator / record_out all recurse over this list.
  // Equality/hashing (DISTINCT/GROUP BY via the value-key path) and eq3/lt3/gt3 are STRUCTURAL
  // (element-wise), recursing into each field's own canonical comparison so a float/decimal/interval
  // field never compares by raw bits (the rule Decimal/Interval already follow).
  | { kind: "composite"; fields: Value[] }
  // A shaped array value (spec/design/array.md §2/§4). Shape is a value property: `dims` holds the
  // per-dimension element counts (row-major), `lbounds` the per-dimension lower bounds (default 1,
  // same length as dims), and `elements` the flattened row-major element values (its length is the
  // product of dims). ndim is dims.length; the empty array is ndim 0 (all arrays empty). Comparison
  // uses PG btree semantics (NULLs comparable and mutually equal — NOT the composite 3VL rule, §5)
  // and, like array_eq/array_cmp, considers dims and lbounds (so [2:4]={1,2,3} ≠ {1,2,3}).
  | { kind: "array"; dims: number[]; lbounds: number[]; elements: Value[] }
  // A range value (spec/design/ranges.md §2/§4) — the distinguished empty range or a non-empty
  // range over a scalar element. `lower`/`upper` are element values; a null bound is
  // unbounded/infinite on that side (and its inclusivity flag is then false). The element type comes
  // from the value's *type*, not stored here (the array precedent). The stored form is CANONICAL
  // (discrete ranges in `[)` form, the empty range normalized — §4), so structural equality on the
  // stored form is the correct value-level equality.
  | {
      kind: "range";
      empty: boolean;
      lower: Value | null;
      upper: Value | null;
      lowerInc: boolean;
      upperInc: boolean;
    }
  // A `json` value (spec/design/json.md §4) — JSON text stored VERBATIM (the original UTF-8 text,
  // preserving whitespace, key order, and duplicate keys), held in `text`. NOT comparable (PG ships
  // no btree/hash opclass — §5); the resolver maps any comparison attempt to 42883. Rendered verbatim
  // (json_out).
  | { kind: "json"; text: string }
  // A `jsonb` value (spec/design/json.md §2) — the canonical tagged-node tree (JsonNode): numbers
  // exact Decimal, object keys deduped + sorted. Comparable by PG's total btree order (§5);
  // equality/ordering go through eq3/lt3/gt3 / the value-key (jsonNodeCmp == 0 IS value equality —
  // the canonical form makes structural equality the value equality). Rendered canonically (jsonb_out).
  | { kind: "jsonb"; node: JsonNode }
  // A `jsonpath` value (spec/design/jsonpath.md, P1a) — the canonical normalized source text. NOT
  // comparable (the resolver maps any comparison to 42883); rendered as its text. Literal-only this
  // slice (a jsonpath column is 0A000), so it never reaches the storage / spill codecs.
  | { kind: "jsonpath"; text: string }
  // An UNFETCHED large-value reference (spec/design/large-values.md §14): a stored
  // external/compressed value loaded as its on-disk pointer instead of being materialized.
  // Internal to the storage/scan layers — the scan layer resolves every column a query
  // touches before the evaluator sees the row, so this kind must never reach a comparison,
  // render, or encode. It is POISONED: those paths throw loudly (an engine bug), never read
  // it as NULL.
  | { kind: "unfetched"; ref: Unfetched };

// The on-disk form of a lazily-loaded value (spec/design/large-values.md §14, generalized to every
// variable-length value by lazy-record.md §5a/L3; spec/fileformat/format.md "Large values") — the
// record's pointer fields (or, for the inline form, a view onto its body bytes), so the scan layer
// can resolve it through the pager (and the cost walk can count its chain pages / decompress slabs)
// without reading the value. `form` is the presence tag: 0x00 inline-deferred (an inline-plain value
// whose decode is deferred — `comp` is the span after the 0x00 tag, kept as a SUBARRAY view of the
// shared faulted page block, FORM (a), zero-copy §5a: the subarray shares (and keeps alive under GC)
// that one block's ArrayBuffer, so a leaf's deferred values share its bytes rather than each owning
// a copy — resident leaf memory ≈ pageSize, §9), 0x02 external-plain / 0x03 inline-compressed / 0x04
// external-compressed the large-value forms; firstPage/storedLen describe the chain for the external
// forms (the payload for plain, the LZ4 block for compressed); rawLen is the decompressed length for
// the compressed forms; comp holds the resident LZ4 block for inline-compressed, or the body-span
// view for inline-deferred. (A value read back from a spill run file owns a fresh copy in `comp` — a
// degenerate form (a), since its page block is long gone — spill.ts.)
export type Unfetched = {
  form: number;
  firstPage: number;
  storedLen: number;
  rawLen: number;
  comp: Uint8Array | undefined;
};

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

// float64Value builds a non-null f64 value (the number IS already binary64; verbatim bits).
export function float64Value(n: number): Value {
  return { kind: "f64", value: n };
}

// float32Value builds a non-null f32 value, rounding to binary32 via Math.fround so the
// stored number is exactly the value the f32 width represents (spec/design/float.md §2).
// Every caller building a f32 — literal, cast, arithmetic result, decode — goes through here
// (or Math.fround directly) so a binary64-precision number can never leak into a f32 value.
export function float32Value(n: number): Value {
  return { kind: "f32", value: Math.fround(n) };
}

// canonFloat maps -0 → +0 for comparison / dedup / key encoding (NOT for storage, which keeps the
// sign bit). NaN is left as-is; floatTotalCmp collapses all NaNs to one equivalence class. This is
// the §3 negative-zero canonicalization: -0 and +0 must dedup to one bucket and key identically.
export function canonFloat(n: number): number {
  return Object.is(n, -0) ? 0 : n;
}

// encodeFloat64Key is the float-order-preserving KEY body for an f64 (spec/design/encoding.md §2.8):
// canonicalize (-0 → +0, every NaN → the one quiet pattern 0x7FF8…000), take the bits big-endian,
// then if the sign bit is set flip ALL 64 bits else flip just the sign bit — mapping the binary64
// TOTAL order (§3, -Inf < finite < +Inf < NaN) onto unsigned byte order. Fixed 8 bytes. -0/+0 and any
// two NaNs collapse to one key, so a UNIQUE float key treats them as one. (The stored VALUE codec
// keeps the bits verbatim — only a NaN is canonicalized — since a value never sorts; format.ts.)
export function encodeFloat64Key(n: number): Uint8Array {
  const dv = new DataView(new ArrayBuffer(8));
  if (Number.isNaN(n)) dv.setBigUint64(0, 0x7ff8000000000000n, false);
  else dv.setFloat64(0, canonFloat(n), false); // canonFloat maps -0 → +0
  let bits = dv.getBigUint64(0, false);
  bits ^= bits >> 63n === 1n ? 0xffffffffffffffffn : 0x8000000000000000n;
  dv.setBigUint64(0, bits, false);
  return new Uint8Array(dv.buffer);
}

// encodeFloat32Key is encodeFloat64Key at binary32 width (4 bytes; canonical NaN 0x7FC00000).
export function encodeFloat32Key(n: number): Uint8Array {
  const dv = new DataView(new ArrayBuffer(4));
  if (Number.isNaN(n)) dv.setUint32(0, 0x7fc00000, false);
  else dv.setFloat32(0, canonFloat(n), false);
  let bits = dv.getUint32(0, false);
  bits = (bits >>> 31 === 1 ? bits ^ 0xffffffff : bits ^ 0x80000000) >>> 0;
  dv.setUint32(0, bits, false);
  return new Uint8Array(dv.buffer);
}

// floatTotalCmp is the float TOTAL order (PostgreSQL's float8 btree order — spec/design/float.md
// §3), NOT raw IEEE: -Infinity < (finite) < +Infinity < NaN, with -0 == +0 and NaN == NaN (all
// NaNs one equivalence class). Returns <0, 0, >0. Raw JS `<`/`===` are wrong here (NaN!==NaN, NaN
// comparisons are all false), so the order is built explicitly. This drives =/</>/<=/>=, ORDER BY,
// MIN/MAX, DISTINCT/GROUP BY, and SUM/AVG's canonical sort for BOTH float widths.
export function floatTotalCmp(a: number, b: number): number {
  const an = Number.isNaN(a);
  const bn = Number.isNaN(b);
  // NaN is the single largest value (above +Infinity); two NaNs are equal.
  if (an || bn) return an && bn ? 0 : an ? 1 : -1;
  // Finite/Infinite: canonicalize -0 → +0 so -0 == +0, then compare numerically. JS `<`/`>` are
  // correct for the finite + ±Infinity range once NaN and -0 are handled.
  const ca = canonFloat(a);
  const cb = canonFloat(b);
  return ca < cb ? -1 : ca > cb ? 1 : 0;
}

// byteaValue builds a non-null bytea value from raw bytes.
export function byteaValue(b: Uint8Array): Value {
  return { kind: "bytea", bytes: b };
}

// uuidValue builds a non-null uuid value from its 16 raw bytes (parseUuid guarantees 16).
export function uuidValue(b: Uint8Array): Value {
  return { kind: "uuid", bytes: b };
}

// timestampValue builds a non-null timestamp from its i64 microsecond instant.
export function timestampValue(m: bigint): Value {
  return { kind: "timestamp", micros: m };
}

// timestamptzValue builds a non-null timestamptz from its i64 microsecond instant.
export function timestamptzValue(m: bigint): Value {
  return { kind: "timestamptz", micros: m };
}

// intervalValue builds a non-null interval value.
export function intervalValue(iv: Interval): Value {
  return { kind: "interval", iv };
}

// dateValue builds a non-null date from its i32 day count since 1970-01-01 (as a bigint).
export function dateValue(days: bigint): Value {
  return { kind: "date", days };
}

// jsonValue builds a non-null json value from its verbatim UTF-8 text (spec/design/json.md §4).
export function jsonValue(text: string): Value {
  return { kind: "json", text };
}

// jsonbValue builds a non-null jsonb value from its canonical node tree (spec/design/json.md §2).
export function jsonbValue(node: JsonNode): Value {
  return { kind: "jsonb", node };
}

// jsonPathValue builds a non-null jsonpath value from its canonical normalized text
// (spec/design/jsonpath.md, P1a).
export function jsonPathValue(text: string): Value {
  return { kind: "jsonpath", text };
}

// compositeValue builds a composite (row) value from its ordered field values (spec/design/composite.md §2).
export function compositeValue(fields: Value[]): Value {
  return { kind: "composite", fields };
}

// arrayValue builds a 1-D array value with the default lower bound 1 (spec/design/array.md §2); an
// empty list is the empty array (ndim 0).
export function arrayValue(elements: Value[]): Value {
  if (elements.length === 0) return { kind: "array", dims: [], lbounds: [], elements: [] };
  return { kind: "array", dims: [elements.length], lbounds: [1], elements };
}

// emptyArray is the empty array `{}` (ndim 0).
export function emptyArray(): Value {
  return { kind: "array", dims: [], lbounds: [], elements: [] };
}

// emptyRangeValue is the empty range (the canonical representation: no bounds, no inclusivity —
// spec/design/ranges.md §4).
export function emptyRangeValue(): Value {
  return { kind: "range", empty: true, lower: null, upper: null, lowerInc: false, upperInc: false };
}

// rangeValue builds a non-empty range value from canonical bounds (a null bound is infinite).
export function rangeValue(
  lower: Value | null,
  upper: Value | null,
  lowerInc: boolean,
  upperInc: boolean,
): Value {
  return { kind: "range", empty: false, lower, upper, lowerInc, upperInc };
}

// arrayNdim is the dimension count of an array value (0 = the empty array).
export function arrayNdim(a: { dims: number[] }): number {
  return a.dims.length;
}

// arrayUbound is the upper bound of dimension d (lb + len - 1).
export function arrayUbound(a: { dims: number[]; lbounds: number[] }, d: number): number {
  return a.lbounds[d] + a.dims[d] - 1;
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
    case "f32":
    case "f64":
      // Native shortest-round-trip (JS Number#toString), the R tag (spec/design/float.md §9).
      // f32's value is already Math.fround'd (binary32), so its toString is the shortest
      // form of the binary32 value. Specials render PG-style. NOTE the JS quirk: (-0).toString()
      // is "0" (and (+0).toString() is "0"), so -0 is special-cased to "-0"; Infinity.toString()
      // is already "Infinity"/"−" handled, and NaN.toString() is "NaN". Layout (exponent
      // threshold) may differ cross-core — absorbed by the R tag's parse-to-value compare.
      return renderFloat(v.value);
    case "bytea":
      return renderByteaHex(v.bytes);
    case "uuid":
      // Canonical 8-4-4-4-12 lowercase-hex form (PG uuid_out).
      return renderUuid(v.bytes);
    case "timestamp":
      return renderTimestamp(v.micros);
    case "timestamptz":
      return renderTimestamptz(v.micros);
    case "date":
      return renderDate(v.days);
    case "interval":
      return renderInterval(v.iv);
    case "composite":
      // A composite renders as PG record_out: `(f1,f2,…)` with per-field quoting
      // (spec/design/composite.md §8). The renderer recurses (a composite field's text is itself
      // quoted because it contains parens/commas).
      return recordOut(v.fields);
    case "array":
      // An array renders as PG array_out: `{e1,e2,…}` (nested braces for a multidim value, an
      // optional `[l:u]=` prefix when any lower bound ≠ 1), with per-element quoting and an unquoted
      // `NULL` for a null element (spec/design/array.md §7).
      return arrayOut(v);
    case "range":
      // A range renders as PG range_out: `empty`, or `[lo,hi)` with bracket/inclusivity, an omitted
      // bound for infinite, and per-bound quoting where the element text has special chars (e.g. a
      // tsrange bound's space — spec/design/ranges.md §5).
      return rangeOut(v);
    case "json":
      // json renders its stored bytes verbatim (json_out — the identity, §4).
      return v.text;
    case "jsonb":
      // jsonb renders the canonical PG text (jsonb_out — §6.2).
      return jsonbOut(v.node);
    case "jsonpath":
      // jsonpath renders its stored canonical normalized text (spec/design/jsonpath.md §2).
      return v.text;
    case "unfetched":
      throw new Error("BUG: unfetched large value escaped the storage layer");
    default:
      return v.int.toString();
  }
}

// recordOut is PostgreSQL record_out (spec/design/composite.md §8): render a composite's fields as
// `(f1,f2,…)`. A NULL field is the empty string between delimiters (unquoted); every other field is
// rendered by its own `render` and double-quoted iff it is empty or contains a delimiter / quote /
// backslash / whitespace. Inside the quotes PostgreSQL DOUBLES an embedded `"` → `""` and an embedded
// `\` → `\\` (NOT backslash-escaping — `record_in` is the exact inverse). Recurses naturally — a
// nested composite's text contains parens/commas, so it is quoted. The spelling must equal PG
// byte-for-byte (CLAUDE.md §8).
export function recordOut(fields: Value[]): string {
  let out = "(";
  for (let i = 0; i < fields.length; i++) {
    if (i > 0) out += ",";
    const f = fields[i]!;
    if (f.kind === "null") continue; // a NULL field is the empty string between delimiters (unquoted)
    const s = render(f);
    if (recordFieldNeedsQuote(s)) {
      out += '"';
      for (const ch of s) {
        // PG doubles `"` and `\` (rowtypes.c record_out): emit the char twice.
        if (ch === '"' || ch === "\\") out += ch;
        out += ch;
      }
      out += '"';
    } else {
      out += s;
    }
  }
  out += ")";
  return out;
}

// recordFieldNeedsQuote reports whether a record_out field token must be double-quoted: the empty
// string, or any token containing a comma, parenthesis, double-quote, backslash, or whitespace
// (C-locale isspace: space, tab, newline, vertical tab \v=0x0b, form feed \f=0x0c, carriage return)
// — PostgreSQL's exact rule.
function recordFieldNeedsQuote(s: string): boolean {
  if (s.length === 0) return true;
  for (const c of s) {
    if (
      c === '"' ||
      c === "\\" ||
      c === "(" ||
      c === ")" ||
      c === "," ||
      c === " " ||
      c === "\t" ||
      c === "\n" ||
      c === "\v" ||
      c === "\f" ||
      c === "\r"
    ) {
      return true;
    }
  }
  return false;
}

// arrayOut is PostgreSQL array_out (spec/design/array.md §7): render an array as `{e1,e2,…}`, with
// nested braces for a multidimensional value and an optional `[l1:u1][l2:u2]=` lower-bound prefix
// when ANY lower bound differs from 1. A NULL element is the unquoted token `NULL`; every other
// element is rendered by its own `render` and double-quoted iff it is empty, equals the literal
// `NULL` (case-insensitive), or contains a delimiter / brace / quote / backslash / whitespace.
// Inside the quotes PostgreSQL BACKSLASH-ESCAPES an embedded `"` → `\"` and `\` → `\\` (the contrast
// with record_out doubling). The empty array renders `{}`. Equals PG byte-for-byte (CLAUDE.md §8).
export function arrayOut(a: { dims: number[]; lbounds: number[]; elements: Value[] }): string {
  if (a.elements.length === 0) return "{}"; // the empty array (ndim 0)
  let out = "";
  if (a.lbounds.some((lb) => lb !== 1)) {
    for (let d = 0; d < a.dims.length; d++) {
      out += `[${a.lbounds[d]}:${arrayUbound(a, d)}]`;
    }
    out += "=";
  }
  const cursor = { i: 0 };
  return out + renderArrayDim(a, 0, cursor);
}

// renderArrayDim renders the brace structure for dimension d of a, consuming flattened elements via
// the cursor (the helper for arrayOut). The innermost dimension renders elements; outer dims recurse.
function renderArrayDim(
  a: { dims: number[]; elements: Value[] },
  d: number,
  cursor: { i: number },
): string {
  let out = "{";
  for (let k = 0; k < a.dims[d]!; k++) {
    if (k > 0) out += ",";
    if (d + 1 === a.dims.length) {
      out += renderArrayElem(a.elements[cursor.i]!);
      cursor.i++;
    } else {
      out += renderArrayDim(a, d + 1, cursor);
    }
  }
  return out + "}";
}

// renderArrayElem renders one array element (PG array_out quoting; a NULL element is unquoted NULL).
function renderArrayElem(e: Value): string {
  if (e.kind === "null") return "NULL";
  const s = render(e);
  if (!arrayElemNeedsQuote(s)) return s;
  let out = '"';
  for (const ch of s) {
    if (ch === '"' || ch === "\\") out += "\\";
    out += ch;
  }
  return out + '"';
}

// arrayElemNeedsQuote reports whether an array_out element token must be double-quoted: the empty
// string, the literal `NULL` (any case — else it would parse back as a NULL element), or any token
// containing a comma, brace, double-quote, backslash, or whitespace — PostgreSQL's exact rule.
function arrayElemNeedsQuote(s: string): boolean {
  if (s.length === 0 || s.toUpperCase() === "NULL") return true;
  for (const c of s) {
    if (
      c === '"' ||
      c === "\\" ||
      c === "{" ||
      c === "}" ||
      c === "," ||
      c === " " ||
      c === "\t" ||
      c === "\n" ||
      c === "\v" ||
      c === "\f" ||
      c === "\r"
    ) {
      return true;
    }
  }
  return false;
}

// ParsedArray is the structured result of parseArrayLiteral: the shape (dims/lbounds) and the
// flattened row-major element tokens (null = a NULL element).
export interface ParsedArray {
  dims: number[];
  lbounds: number[];
  tokens: (string | null)[];
}

// ArrayInResult is parseArrayLiteral's result: a parsed array, or a classified failure —
// "malformed" (→ 22P02) or "boundflip" (a declared [l:u] with u<l → 2202E).
export type ArrayInResult =
  | { ok: true; value: ParsedArray }
  | { ok: false; err: "malformed" | "boundflip" };

// ArrNode is a parsed brace node: a leaf scalar token (leaf, null = the NULL token) or a braced level.
type ArrNode = { leaf: string | null; isLeaf: true } | { children: ArrNode[]; isLeaf: false };

// parseArrayLiteral is PostgreSQL array_in (spec/design/array.md §7) — the inverse of arrayOut. It
// parses an optional dimension prefix `[l1:u1][l2:u2]…=`, then a (possibly nested) brace structure
// `{…}`, returning the shape (dims/lbounds) and flattened row-major raw element tokens (no coercion).
// An element is quoted (`"…"`, `\"`→`"`, `\\`→`\`) or unquoted (to the next top-level `,`/`}`,
// whitespace trimmed, `\x`→`x`); an unquoted `NULL` (any case) is a NULL element (null), a quoted
// `"NULL"` the 4-char string. `{}` is the empty array (ndim 0). A multidim literal must be
// rectangular and, if a prefix is given, the contents must match the declared dims (else
// "malformed"); a prefix with u<l is "boundflip".
export function parseArrayLiteral(input: string): ArrayInResult {
  const s = input.replace(/^[\t\n\v\f\r ]+/, "").replace(/[\t\n\v\f\r ]+$/, "");
  const p = new ArrParser(s);
  const malformed: ArrayInResult = { ok: false, err: "malformed" };

  const prefixLb: number[] = [];
  const prefixDims: number[] = [];
  if (p.peek() === "[") {
    while (p.peek() === "[") {
      p.bump(); // [
      const lb = p.parseInt();
      if (lb === null) return malformed;
      if (p.peek() !== ":") return malformed;
      p.bump(); // :
      const ub = p.parseInt();
      if (ub === null) return malformed;
      if (p.peek() !== "]") return malformed;
      p.bump(); // ]
      if (ub < lb) return { ok: false, err: "boundflip" };
      prefixLb.push(lb);
      prefixDims.push(ub - lb + 1);
    }
    if (p.peek() !== "=") return malformed;
    p.bump(); // =
    p.skipWs();
  }

  const node = p.parseNode();
  if (node === null) return malformed;
  p.skipWs();
  if (!p.atEnd()) return malformed; // trailing junk
  if (node.isLeaf) return malformed; // a literal must start with a brace
  // The bare top-level empty brace `{}` is the empty array (ndim 0).
  if (node.children.length === 0) {
    if (prefixDims.length !== 0) return malformed;
    return { ok: true, value: { dims: [], lbounds: [], tokens: [] } };
  }
  const dims = nodeDims(node);
  if (dims === null || dims.length > 6) return malformed;
  const tokens: (string | null)[] = [];
  flattenNodes(node, tokens);
  let lbounds: number[];
  if (prefixDims.length === 0) {
    lbounds = dims.map(() => 1);
  } else {
    if (prefixDims.length !== dims.length || prefixDims.some((d, i) => d !== dims[i]))
      return malformed;
    lbounds = prefixLb;
  }
  return { ok: true, value: { dims, lbounds, tokens } };
}

// ArrParser is a string cursor for parseArrayLiteral.
class ArrParser {
  private s: string;
  private i = 0;
  constructor(s: string) {
    this.s = s;
  }
  peek(): string | undefined {
    return this.s[this.i];
  }
  bump(): void {
    this.i++;
  }
  atEnd(): boolean {
    return this.i >= this.s.length;
  }
  skipWs(): void {
    while (this.i < this.s.length && isArrWs(this.s[this.i]!)) this.i++;
  }
  // parseInt parses a signed decimal integer (a dimension bound); null on no digits / bad number.
  parseInt(): number | null {
    let buf = "";
    if (this.peek() === "-") {
      buf += "-";
      this.bump();
    }
    while (this.i < this.s.length && this.s[this.i]! >= "0" && this.s[this.i]! <= "9") {
      buf += this.s[this.i]!;
      this.bump();
    }
    if (buf === "" || buf === "-") return null;
    const n = Number(buf);
    return Number.isInteger(n) ? n : null;
  }
  // parseNode parses one element: a nested `{…}` (a braced level) or a scalar token (a leaf).
  parseNode(): ArrNode | null {
    this.skipWs();
    if (this.peek() === "{") {
      this.bump(); // {
      this.skipWs();
      const children: ArrNode[] = [];
      if (this.peek() === "}") {
        this.bump(); // empty braces
        return { children, isLeaf: false };
      }
      for (;;) {
        const child = this.parseNode();
        if (child === null) return null;
        children.push(child);
        this.skipWs();
        const c = this.peek();
        if (c === undefined) return null;
        this.bump();
        if (c === ",") continue;
        if (c === "}") break;
        return null;
      }
      return { children, isLeaf: false };
    }
    const tok = this.parseScalar();
    if (tok === undefined) return null;
    return { leaf: tok, isLeaf: true };
  }
  // parseScalar parses one scalar token (quoted or unquoted); null is the unquoted NULL token,
  // undefined signals a malformed token.
  parseScalar(): string | null | undefined {
    let buf = "";
    if (this.peek() === '"') {
      this.bump(); // opening quote
      for (;;) {
        const c = this.peek();
        if (c === undefined) return undefined; // unterminated
        this.bump();
        if (c === '"') break;
        if (c === "\\") {
          const c2 = this.peek();
          if (c2 === undefined) return undefined;
          buf += c2;
          this.bump();
        } else {
          buf += c;
        }
      }
      return buf;
    }
    // Unquoted: read until a top-level `,`/`}`/`{`, processing `\x`→`x`.
    for (;;) {
      const c = this.peek();
      if (c === undefined) return undefined;
      if (c === "," || c === "}" || c === "{") break;
      if (c === "\\") {
        this.bump();
        const c2 = this.peek();
        if (c2 === undefined) return undefined;
        buf += c2;
        this.bump();
      } else {
        buf += c;
        this.bump();
      }
    }
    const trimmed = buf.replace(/^[\t\n\v\f\r ]+/, "").replace(/[\t\n\v\f\r ]+$/, "");
    if (trimmed.length === 0) return undefined; // a bare empty unquoted element is malformed (PG)
    return trimmed.toUpperCase() === "NULL" ? null : trimmed;
  }
}

function isArrWs(c: string): boolean {
  return c === " " || c === "\t" || c === "\n" || c === "\v" || c === "\f" || c === "\r";
}

// nodeDims returns the dimensions of a parsed brace node (recursing), or null if non-rectangular
// (all sub-arrays at a level must share the same shape and kind — a leaf-vs-array mix is malformed).
function nodeDims(node: ArrNode): number[] | null {
  if (node.isLeaf) return [];
  if (node.children.length === 0) return null; // a nested empty brace is not a valid sub-array
  const child0 = nodeDims(node.children[0]!);
  if (child0 === null) return null;
  for (const c of node.children.slice(1)) {
    const cd = nodeDims(c);
    if (cd === null || cd.length !== child0.length || cd.some((d, i) => d !== child0[i]))
      return null;
  }
  return [node.children.length, ...child0];
}

// flattenNodes collects the leaf tokens of a parsed brace node in row-major order (left-to-right DFS).
function flattenNodes(node: ArrNode, out: (string | null)[]): void {
  if (node.isLeaf) {
    out.push(node.leaf);
    return;
  }
  for (const c of node.children) flattenNodes(c, out);
}

// parseRecordTokens is the PostgreSQL record_in tokenizer (spec/design/composite.md §8) — the exact
// inverse of recordOut. It splits the text of a composite literal `(f1,f2,…)` into its raw field
// tokens WITHOUT type coercion: the caller (the executor) coerces each token to its field type. A
// field is either quoted (`"…"` with `""`→`"` and `\x`→`x` un-escaping) or unquoted (read literally
// up to the next top-level `,`/`)`, with `\x`→`x`); an UNQUOTED EMPTY field is SQL-NULL (null), a
// quoted empty field is the empty string (""). Surrounding ASCII whitespace around the whole literal
// is ignored; whitespace INSIDE an unquoted token is preserved (PG leaves trimming to each field's
// input function). Returns null on a malformed literal — the executor maps that to 22P02 (kept
// error-free so this module need not depend on the error type). A per-field null means SQL-NULL.
export function parseRecordTokens(input: string): (string | null)[] | null {
  const s = input.replace(/^[\t\n\v\f\r ]+/, "").replace(/[\t\n\v\f\r ]+$/, "");
  let pos = 0;
  const isWs = (c: string): boolean =>
    c === " " || c === "\t" || c === "\n" || c === "\v" || c === "\f" || c === "\r";
  if (s[pos] !== "(") return null;
  pos++;
  const fields: (string | null)[] = [];
  for (;;) {
    let buf = "";
    let quoted = false;
    let present = false;
    if (s[pos] === '"') {
      quoted = true;
      present = true;
      pos++; // opening quote
      for (;;) {
        if (pos >= s.length) return null; // unterminated quoted field
        const c = s[pos]!;
        if (c === '"') {
          pos++;
          if (s[pos] === '"') {
            pos++;
            buf += '"'; // doubled quote → one quote
          } else {
            break; // closing quote
          }
        } else if (c === "\\") {
          pos++;
          if (pos >= s.length) return null;
          buf += s[pos]!;
          pos++;
        } else {
          buf += c;
          pos++;
        }
      }
      // A quoted field may be followed by ASCII whitespace before the delimiter (PG).
      while (pos < s.length && isWs(s[pos]!)) pos++;
    } else {
      // Unquoted: read literally until a top-level `,`/`)`, processing `\x`→`x`.
      for (;;) {
        if (pos >= s.length) return null; // missing ')'
        const c = s[pos]!;
        if (c === "," || c === ")") break;
        if (c === "\\") {
          pos++;
          if (pos >= s.length) return null;
          buf += s[pos]!;
          present = true;
          pos++;
        } else {
          buf += c;
          present = true;
          pos++;
        }
      }
    }
    // An unquoted empty field is SQL-NULL; a quoted (even empty) field is the string.
    fields.push(present || quoted ? buf : null);
    const d = s[pos];
    if (d === ",") {
      pos++;
      continue;
    }
    if (d === ")") {
      pos++;
      break;
    }
    return null;
  }
  // Nothing but trailing nothing may follow the closing ')'.
  if (pos !== s.length) return null;
  return fields;
}

// renderFloat formats a float value's JS number to its conformance text (spec/design/float.md §9):
// the native shortest-round-trip (Number#toString) for finite values, PG-style spellings for the
// specials. The R tag compares by parsed value, so the only requirement here is that the digits be
// shortest-round-trip (mathematically unique cross-core) and the specials match PG's words. The two
// JS quirks handled: (-0).toString() is "0" (force "-0"), and NaN/Infinity already toString to
// "NaN"/"Infinity"/"-Infinity".
export function renderFloat(n: number): string {
  if (Object.is(n, -0)) return "-0";
  return n.toString();
}

// eq3 is three-valued equality. NULL compared with anything (including NULL) is
// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers compare by
// value (all integer types promote losslessly into the common bigint); text by the C
// collation (UTF-8 byte order — string equality is exact for ===); booleans by value
// (false < true). A mixed cross-family pair never reaches here (the resolver rejects it,
// 42804); any other variant pair is a NULL.
export function eq3(a: Value, b: Value): ThreeValued {
  // Poisoned (large-values.md §14): an unfetched value must never be compared — falling
  // through to UNKNOWN would silently read it as NULL.
  if (a.kind === "unfetched" || b.kind === "unfetched") {
    throw new Error("BUG: unfetched large value escaped the storage layer");
  }
  if (a.kind === "null" || b.kind === "null") return "unknown";
  const c = numericCmp(a, b);
  if (c !== undefined) return bool3(c === 0);
  // Floats compare by the TOTAL order (NaN == NaN TRUE, -0 == +0). A mixed-width float pair never
  // reaches here — the resolver promotes both to f64 first. float is a strict island, so a
  // float never compares with int/decimal (resolver rejects 42804).
  if (a.kind === "f32" && b.kind === "f32") return bool3(floatTotalCmp(a.value, b.value) === 0);
  if (a.kind === "f64" && b.kind === "f64") return bool3(floatTotalCmp(a.value, b.value) === 0);
  if (a.kind === "text" && b.kind === "text") return bool3(a.text === b.text);
  if (a.kind === "bytea" && b.kind === "bytea") return bool3(compareBytea(a.bytes, b.bytes) === 0);
  if (a.kind === "uuid" && b.kind === "uuid") return bool3(compareBytea(a.bytes, b.bytes) === 0);
  if (a.kind === "bool" && b.kind === "bool") return bool3(a.value === b.value);
  // Timestamps compare by the i64 instant (infinity is just an extreme value).
  if (a.kind === "timestamp" && b.kind === "timestamp") return bool3(a.micros === b.micros);
  if (a.kind === "timestamptz" && b.kind === "timestamptz") return bool3(a.micros === b.micros);
  if (a.kind === "date" && b.kind === "date") return bool3(a.days === b.days);
  // Intervals compare by the canonical 128-bit span (spec/design/interval.md §2).
  if (a.kind === "interval" && b.kind === "interval") return bool3(intervalCmp(a.iv, b.iv) === 0);
  // Composite `=` is element-wise 3VL (PG row comparison, spec/design/composite.md §5): FALSE if any
  // field is FALSE; else UNKNOWN if any field is UNKNOWN; else TRUE. So a FALSE field dominates a
  // NULL field. Arity matches (the resolver only compares two composites of the same type). The
  // recursion bottoms out in the field comparators.
  if (a.kind === "composite" && b.kind === "composite") {
    let anyUnknown = false;
    for (let i = 0; i < a.fields.length; i++) {
      const r = eq3(a.fields[i]!, b.fields[i]!);
      if (r === "false") return "false";
      if (r === "unknown") anyUnknown = true;
    }
    return anyUnknown ? "unknown" : "true";
  }
  // Array `=` uses PG btree semantics (spec/design/array.md §5), NOT the composite 3VL rule: same
  // length and every element pair equal-or-both-NULL → TRUE, else FALSE. NULL elements are
  // comparable and mutually equal, so the result is ALWAYS definite (never UNKNOWN).
  if (a.kind === "array" && b.kind === "array") return bool3(arrayEqual(a, b));
  // Range `=` is structural over the canonical form (PG range btree, NOT 3VL): two canonical ranges
  // are equal iff rangeTotalCmp is 0, always definite (spec/design/ranges.md §6). NULLs propagate
  // only at the whole-value level (handled above), never per-bound.
  if (a.kind === "range" && b.kind === "range") return bool3(rangeTotalCmp(a, b) === 0);
  // jsonb `=` is structural over the canonical tree — always definite (PG btree, not 3VL; no SQL
  // NULLs inside a document, §5). Consistent with jsonNodeCmp == 0. (json never reaches here — the
  // resolver maps any json comparison to 42883; jsonb comparison resolves in J2.)
  if (a.kind === "jsonb" && b.kind === "jsonb") return bool3(jsonNodeCmp(a.node, b.node) === 0);
  return "unknown";
}

// ArrayShape is the structural view of an array value (its shape + flattened elements).
type ArrayShape = { dims: number[]; lbounds: number[]; elements: Value[] };

function numArrEqual(a: number[], b: number[]): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i]);
}

// arrayEqual is PG array_eq (spec/design/array.md §5): same dimensionality AND lower bounds AND
// every element pair equal, where two NULL elements are mutually equal (NOT 3VL). So [2:4]={1,2,3}
// and {1,2,3} are not equal. Always definite.
function arrayEqual(a: ArrayShape, b: ArrayShape): boolean {
  if (!numArrEqual(a.dims, b.dims) || !numArrEqual(a.lbounds, b.lbounds)) return false;
  for (let i = 0; i < a.elements.length; i++) {
    // btree NULL semantics: an element pair is equal iff its total order is 0 — NULL elements are
    // comparable and mutually equal, and a composite element recurses through the composite total
    // order (NULLs-last per field), NOT the 3VL eq3 (which is UNKNOWN for a NULL field). This is the
    // array-of-composite fix (spec/design/array.md §5).
    if (elemTotalCmp(a.elements[i]!, b.elements[i]!) !== 0) return false;
  }
  return true;
}

// arrayTotalCmp is the PG array_cmp total order over two arrays (spec/design/array.md §5): walk the
// flattened element pairs (the first non-equal pair decides), then fewer total elements first, then
// smaller ndim, then per dimension smaller length and smaller lower bound. NULL elements are
// comparable — NULL sorts AFTER every non-NULL and two NULLs are equal (NULLs-last). Always definite.
function arrayTotalCmp(a: ArrayShape, b: ArrayShape): number {
  const n = Math.min(a.elements.length, b.elements.length);
  for (let i = 0; i < n; i++) {
    const c = elemTotalCmp(a.elements[i]!, b.elements[i]!);
    if (c !== 0) return c;
  }
  if (a.elements.length !== b.elements.length)
    return a.elements.length < b.elements.length ? -1 : 1;
  if (a.dims.length !== b.dims.length) return a.dims.length < b.dims.length ? -1 : 1;
  for (let d = 0; d < a.dims.length; d++) {
    if (a.dims[d] !== b.dims[d]) return a.dims[d]! < b.dims[d]! ? -1 : 1;
    if (a.lbounds[d] !== b.lbounds[d]) return a.lbounds[d]! < b.lbounds[d]! ? -1 : 1;
  }
  return 0;
}

// elemTotalCmp is a total order over two array elements with NULL the largest value (NULLs-last)
// and two NULLs equal. A composite element recurses through the composite total order (NULLs-last
// per field) and a nested array through arrayTotalCmp — NOT the composite 3VL eq3/lt3, which can be
// UNKNOWN for a NULL field and would break array comparison's "always a definite boolean" guarantee
// (spec/design/array.md §5 — the array-of-composite subtlety; this must agree with valueCmp, the
// ORDER BY path). A present scalar element uses its definite eq3/lt3.
function elemTotalCmp(x: Value, y: Value): number {
  const xn = x.kind === "null";
  const yn = y.kind === "null";
  if (xn && yn) return 0;
  if (xn) return 1; // NULL sorts last
  if (yn) return -1;
  if (x.kind === "composite" && y.kind === "composite")
    return compositeTotalCmp(x.fields, y.fields);
  if (x.kind === "array" && y.kind === "array") return arrayTotalCmp(x, y);
  if (eq3(x, y) === "true") return 0;
  return lt3(x, y) === "true" ? -1 : 1;
}

// compositeTotalCmp is the total order over two composite values of the same type: lexicographic
// over fields, each compared by elemTotalCmp (so a NULL field sorts last and two NULL fields are
// equal — the composite sort key, NOT the 3VL row comparison), with a field-count tiebreak for
// totality. Kept identical to the composite ORDER BY key (valueCmp's composite arm) so the array
// `<` operator and ORDER BY never disagree (spec/design/array.md §5).
function compositeTotalCmp(a: Value[], b: Value[]): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    const c = elemTotalCmp(a[i]!, b[i]!);
    if (c !== 0) return c;
  }
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
}

// compositeOrder3 is three-valued lexicographic row ordering (PG row comparison,
// spec/design/composite.md §5), shared by lt3 (gt = false) and gt3 (gt = true): walk fields; the
// first whose `=` is FALSE decides via that field's `<`/`>`; the first whose `=` is UNKNOWN (a NULL
// operand) makes the whole comparison UNKNOWN; all-equal rows are neither `<` nor `>` (FALSE). Arity
// matches (same composite type — the resolver's gate).
function compositeOrder3(a: Value[], b: Value[], gt: boolean): ThreeValued {
  for (let i = 0; i < a.length; i++) {
    const r = eq3(a[i]!, b[i]!);
    if (r === "true") continue;
    if (r === "false") return gt ? gt3(a[i]!, b[i]!) : lt3(a[i]!, b[i]!);
    return "unknown"; // r === "unknown"
  }
  return "false";
}

// lt3 is the three-valued ordering predicate a < b (numerics by value with int↔decimal
// promotion; text by C collation = UTF-8 byte order; boolean by value, false < true).
export function lt3(a: Value, b: Value): ThreeValued {
  // Poisoned (large-values.md §14): an unfetched value must never be compared — falling
  // through to UNKNOWN would silently read it as NULL.
  if (a.kind === "unfetched" || b.kind === "unfetched") {
    throw new Error("BUG: unfetched large value escaped the storage layer");
  }
  if (a.kind === "null" || b.kind === "null") return "unknown";
  const c = numericCmp(a, b);
  if (c !== undefined) return bool3(c < 0);
  if (a.kind === "f32" && b.kind === "f32") return bool3(floatTotalCmp(a.value, b.value) < 0);
  if (a.kind === "f64" && b.kind === "f64") return bool3(floatTotalCmp(a.value, b.value) < 0);
  if (a.kind === "text" && b.kind === "text") return bool3(compareTextC(a.text, b.text) < 0);
  if (a.kind === "bytea" && b.kind === "bytea") return bool3(compareBytea(a.bytes, b.bytes) < 0);
  if (a.kind === "uuid" && b.kind === "uuid") return bool3(compareBytea(a.bytes, b.bytes) < 0);
  if (a.kind === "bool" && b.kind === "bool") return bool3(!a.value && b.value);
  if (a.kind === "timestamp" && b.kind === "timestamp") return bool3(a.micros < b.micros);
  if (a.kind === "timestamptz" && b.kind === "timestamptz") return bool3(a.micros < b.micros);
  if (a.kind === "date" && b.kind === "date") return bool3(a.days < b.days);
  if (a.kind === "interval" && b.kind === "interval") return bool3(intervalCmp(a.iv, b.iv) < 0);
  // Composite `<` is lexicographic with PG row-comparison NULL propagation (spec/design/composite.md
  // §5): the first field that is not equal decides via its own `<`; a field whose `=` is UNKNOWN (a
  // NULL operand) makes the whole comparison UNKNOWN; all-equal rows are not `<`.
  if (a.kind === "composite" && b.kind === "composite")
    return compositeOrder3(a.fields, b.fields, false);
  // Array `<` uses the PG array_cmp total order (spec/design/array.md §5): element-wise, NULL after
  // every non-NULL, shorter prefix first. Always definite.
  if (a.kind === "array" && b.kind === "array") return bool3(arrayTotalCmp(a, b) < 0);
  // Range `<` uses the PG range_cmp total order (spec/design/ranges.md §6): `empty` below every
  // non-empty range, then by lower bound, then by upper bound. Always definite.
  if (a.kind === "range" && b.kind === "range") return bool3(rangeTotalCmp(a, b) < 0);
  // jsonb `<` uses PG's total btree order (spec/design/json.md §5): type rank, then per-kind ordering
  // (containers by count first). Always definite, never UNKNOWN.
  if (a.kind === "jsonb" && b.kind === "jsonb") return bool3(jsonNodeCmp(a.node, b.node) < 0);
  return "unknown";
}

// gt3 is the three-valued ordering predicate a > b (numerics by value with int↔decimal
// promotion; text by C collation = UTF-8 byte order; boolean by value, false < true).
export function gt3(a: Value, b: Value): ThreeValued {
  // Poisoned (large-values.md §14): an unfetched value must never be compared — falling
  // through to UNKNOWN would silently read it as NULL.
  if (a.kind === "unfetched" || b.kind === "unfetched") {
    throw new Error("BUG: unfetched large value escaped the storage layer");
  }
  if (a.kind === "null" || b.kind === "null") return "unknown";
  const c = numericCmp(a, b);
  if (c !== undefined) return bool3(c > 0);
  if (a.kind === "f32" && b.kind === "f32") return bool3(floatTotalCmp(a.value, b.value) > 0);
  if (a.kind === "f64" && b.kind === "f64") return bool3(floatTotalCmp(a.value, b.value) > 0);
  if (a.kind === "text" && b.kind === "text") return bool3(compareTextC(a.text, b.text) > 0);
  if (a.kind === "bytea" && b.kind === "bytea") return bool3(compareBytea(a.bytes, b.bytes) > 0);
  if (a.kind === "uuid" && b.kind === "uuid") return bool3(compareBytea(a.bytes, b.bytes) > 0);
  if (a.kind === "bool" && b.kind === "bool") return bool3(a.value && !b.value);
  if (a.kind === "timestamp" && b.kind === "timestamp") return bool3(a.micros > b.micros);
  if (a.kind === "timestamptz" && b.kind === "timestamptz") return bool3(a.micros > b.micros);
  if (a.kind === "date" && b.kind === "date") return bool3(a.days > b.days);
  if (a.kind === "interval" && b.kind === "interval") return bool3(intervalCmp(a.iv, b.iv) > 0);
  // Composite `>` — the lexicographic mirror of `<` (spec/design/composite.md §5).
  if (a.kind === "composite" && b.kind === "composite")
    return compositeOrder3(a.fields, b.fields, true);
  // Array `>` — the total-order mirror of `<` (spec/design/array.md §5).
  if (a.kind === "array" && b.kind === "array") return bool3(arrayTotalCmp(a, b) > 0);
  // Range `>` — the total-order mirror of `<` (spec/design/ranges.md §6).
  if (a.kind === "range" && b.kind === "range") return bool3(rangeTotalCmp(a, b) > 0);
  // jsonb `>` — the total-order mirror of `<` (spec/design/json.md §5).
  if (a.kind === "jsonb" && b.kind === "jsonb") return bool3(jsonNodeCmp(a.node, b.node) > 0);
  return "unknown";
}

// valueEqual is value-level (NULL-safe) structural equality — the basis for DISTINCT/GROUP BY
// dedup and notDistinctFrom (spec/design/composite.md §5). NULL == NULL is TRUE here (unlike the
// 3VL eq3); a composite recurses element-wise; every other variant reduces to eq3 (definite when
// neither side is NULL). NOT used for the WHERE/3VL paths.
export function valueEqual(a: Value, b: Value): boolean {
  if (a.kind === "null" || b.kind === "null") return a.kind === "null" && b.kind === "null";
  if (a.kind === "composite" && b.kind === "composite") {
    if (a.fields.length !== b.fields.length) return false;
    for (let i = 0; i < a.fields.length; i++) {
      if (!valueEqual(a.fields[i]!, b.fields[i]!)) return false;
    }
    return true;
  }
  // Two arrays are "not distinct" iff structurally equal (btree equality; NULL elements mutually
  // equal — spec/design/array.md §5).
  if (a.kind === "array" && b.kind === "array") return arrayEqual(a, b);
  // Two ranges are "not distinct" iff structurally equal over the canonical form (the same equality
  // as `==`/`eq3` — rangeTotalCmp 0; spec/design/ranges.md §6).
  if (a.kind === "range" && b.kind === "range") return rangeTotalCmp(a, b) === 0;
  return eq3(a, b) === "true";
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
  // Two composites are "not distinct" iff structurally equal — NULL-safe, so a NULL field equals a
  // NULL field (the value-level equality, not the 3VL eq3).
  if (a.kind === "composite" && b.kind === "composite") return valueEqual(a, b);
  return eq3(a, b) === "true";
}

// isNullTest is PostgreSQL's `IS [NOT] NULL` test (spec/design/composite.md §5) — for a composite
// these are **not** negations of each other, they are the all-fields rule, and it is **one level
// deep, NOT recursive** (the empirically-probed PG 18 behavior — the differential oracle). A field
// counts as "null" only if it is itself SQL-NULL; a *composite-valued* field is a non-null value, so
// it counts as PRESENT and is not descended into. negated = false (IS NULL): TRUE iff this value is
// SQL-NULL OR every immediate field is SQL-NULL. negated = true (IS NOT NULL): TRUE iff this value is
// non-NULL AND every immediate field is non-SQL-NULL. So `ROW(1, NULL)` is FALSE for both, and
// `ROW(ROW(NULL,NULL), ROW(NULL,NULL)) IS NULL` is FALSE (the inner rows are non-null values). A
// scalar follows the ordinary rule. Always definite.
export function isNullTest(v: Value, negated: boolean): boolean {
  if (v.kind === "composite") {
    return negated
      ? // IS NOT NULL: every immediate field is a non-(SQL-)NULL value.
        v.fields.every((f) => f.kind !== "null")
      : // IS NULL: every immediate field is SQL-NULL (a composite field is NOT).
        v.fields.every((f) => f.kind === "null");
  }
  // A whole-value NULL: IS NULL → true, IS NOT NULL → false. Any present scalar is the inverse.
  if (v.kind === "null") return !negated;
  return negated;
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
