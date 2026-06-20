// Order-preserving key encoding (CLAUDE.md §8; spec/design/encoding.md). Encoded keys
// sort byte-for-byte identically to logical order, so stored key order needs no
// comparator. Method int-be-signflip: fixed-width big-endian with the sign bit
// inverted (add bias 2^(bits-1), emit unsigned BE). Verified byte-for-byte against
// spec/encoding/integers.toml in tests — this is what guarantees the Rust, Go, and TS
// cores iterate keys identically.
//
// Values are `bigint` (i64 keys exceed JS's safe-integer range); the byte marshalling
// itself runs width-specialized in the number domain where exact (≤ 32-bit halves), since
// per-byte bigint arithmetic allocates and these codecs sit on the hot path.

import { type ScalarType, widthBytes } from "./types.ts";

// encodeInt encodes a non-null integer value of the given type to its order-preserving
// key bytes — value + 2^(bits-1), emitted as `width` unsigned big-endian bytes. value is
// assumed in range for t (callers range-check). Width-specialized: the 8-byte path writes
// the biased value as two number-domain 32-bit halves, the narrow path stays in the number
// domain entirely — per-byte bigint arithmetic is allocation-heavy in V8 and this sits on
// the key-encode/leaf-decode hot path.
export function encodeInt(t: ScalarType, value: bigint): Uint8Array {
  const width = widthBytes(t);
  const out = new Uint8Array(width);
  if (width === 8) {
    const u = BigInt.asUintN(64, value + 9223372036854775808n);
    let hi = Number(u >> 32n);
    let lo = Number(u & 0xffffffffn);
    for (let i = 3; i >= 0; i--) {
      out[i] = hi & 0xff;
      hi >>>= 8;
      out[i + 4] = lo & 0xff;
      lo >>>= 8;
    }
    return out;
  }
  // width ≤ 4: the biased value is in [0, 2^32), exact in a JS number.
  let u = Number(value) + 2 ** (width * 8 - 1);
  for (let i = width - 1; i >= 0; i--) {
    out[i] = u & 0xff;
    u = Math.floor(u / 256);
  }
  return out;
}

// decodeInt is the inverse of encodeInt. b.length must equal the type's width.
export function decodeInt(t: ScalarType, b: Uint8Array): bigint {
  return decodeIntAt(t, b, 0);
}

// decodeIntAt decodes an encoded integer in place at offset `off` — no subarray view, so the
// record decoder reads values straight off the page buffer (the leaf-fault hot path). Width-
// specialized like encodeInt.
export function decodeIntAt(t: ScalarType, b: Uint8Array, off: number): bigint {
  const width = widthBytes(t);
  if (width === 8) {
    const hi = ((b[off]! << 24) | (b[off + 1]! << 16) | (b[off + 2]! << 8) | b[off + 3]!) >>> 0;
    const lo =
      ((b[off + 4]! << 24) | (b[off + 5]! << 16) | (b[off + 6]! << 8) | b[off + 7]!) >>> 0;
    return ((BigInt(hi) << 32n) | BigInt(lo)) - 9223372036854775808n;
  }
  let u = 0;
  for (let i = 0; i < width; i++) {
    u = u * 256 + b[off + i]!;
  }
  return BigInt(u - 2 ** (width * 8 - 1));
}

// encodeBool encodes a non-null boolean to its order-preserving key body: a single bool-byte,
// 0x00 for false < 0x01 for true (method bool-byte, spec/design/encoding.md §2.9). Fixed-width 1,
// so self-delimiting with no sign-flip / escape / terminator — like uuid. Byte-identical to the
// boolean value-codec body (a stored boolean reuses these bytes behind the §2.2 presence tag —
// spec/fileformat/format.md). A PK is NOT NULL, so no presence tag.
export function encodeBool(value: boolean): Uint8Array {
  return new Uint8Array([value ? 0x01 : 0x00]);
}

// encodeTerminated encodes a non-null text/bytea value to its order-preserving key body (method
// text-terminated-escape / bytea-terminated-escape, spec/design/encoding.md §2.4/§2.6). content is
// the value's raw bytes — UTF-8 for text (the C collation, so byte order equals code-point order),
// raw bytes for bytea. Variable-width, so it must be self-delimiting: escape every 0x00 to
// 0x00 0xFF and terminate with 0x00 0x01. The terminator is the only place a 0x00 is followed by a
// byte < 0xFF, so it sorts below any real continuation — a value sorts before any value that extends
// it. A PK is NOT NULL, so the stored key is this bare body with no presence tag.
export function encodeTerminated(content: Uint8Array): Uint8Array {
  const out: number[] = [];
  for (const b of content) {
    out.push(b);
    if (b === 0x00) out.push(0xff);
  }
  out.push(0x00, 0x01);
  return Uint8Array.from(out);
}

// encodeNullable encodes a nullable key slot: a 1-byte presence tag (0x00 present, 0x01
// NULL), with the value bytes following the tag when present. Because 0x00 < 0x01,
// present values sort before NULL, so NULLs sort LAST in ascending order; descending
// inverts the component, lifting NULL to first (the PostgreSQL model — NULL is the
// largest value; spec/design/encoding.md §2/§4). `null` means NULL.
export function encodeNullable(t: ScalarType, value: bigint | null): Uint8Array {
  if (value === null) return new Uint8Array([0x01]);
  const v = encodeInt(t, value);
  const out = new Uint8Array(1 + v.length);
  out[0] = 0x00;
  out.set(v, 1);
  return out;
}
