// Order-preserving key encoding (CLAUDE.md §8; spec/design/encoding.md). Encoded keys
// sort byte-for-byte identically to logical order, so stored key order needs no
// comparator. Method int-be-signflip: fixed-width big-endian with the sign bit
// inverted (add bias 2^(bits-1), emit unsigned BE). Verified byte-for-byte against
// spec/encoding/integers.toml in tests — this is what guarantees the Rust, Go, and TS
// cores iterate keys identically.
//
// All arithmetic is `bigint`: int64 keys exceed JS's safe-integer range, and a single
// bigint path is exact at every width.

import { type ScalarType, widthBytes } from "./types.ts";

// encodeInt encodes a non-null integer value of the given type to its order-preserving
// key bytes. value is assumed in range for t (callers range-check).
export function encodeInt(t: ScalarType, value: bigint): Uint8Array {
  const width = widthBytes(t);
  const bits = BigInt(width * 8);
  const bias = 1n << (bits - 1n);
  const mask = (1n << bits) - 1n;
  // value + 2^(bits-1), kept to `width` bytes. For an in-range value the sum is in
  // [0, 2^bits), so the mask is a no-op; it documents the fixed width.
  const u = (value + bias) & mask;
  const out = new Uint8Array(width);
  for (let i = 0; i < width; i++) {
    out[width - 1 - i] = Number((u >> (8n * BigInt(i))) & 0xffn);
  }
  return out;
}

// decodeInt is the inverse of encodeInt. b.length must equal the type's width.
export function decodeInt(t: ScalarType, b: Uint8Array): bigint {
  const width = widthBytes(t);
  const bias = 1n << (BigInt(width * 8) - 1n);
  let u = 0n;
  for (const x of b) {
    u = (u << 8n) | BigInt(x);
  }
  return u - bias;
}

// encodeNullable encodes a nullable key slot: a 1-byte presence tag (0x00 NULL, 0x01
// present) followed by the value bytes when present. Makes NULLs sort first in
// ascending order (spec/design/encoding.md §2/§4). `null` means NULL.
export function encodeNullable(t: ScalarType, value: bigint | null): Uint8Array {
  if (value === null) return new Uint8Array([0x00]);
  const v = encodeInt(t, value);
  const out = new Uint8Array(1 + v.length);
  out[0] = 0x01;
  out.set(v, 1);
  return out;
}
