// The compact leaf's size arithmetic (format_version 52 — spec/fileformat/format.md "Leaf node" and
// "Fan-out"; spec/design/leaf-structural.md). Pure byte and integer rules with no dependency on the
// value codec, so both the B+tree (pmap.ts) and the leaf codec (format.ts) import them without a
// cycle. Every rule here is part of the byte contract: a leaf's payload is
//
//   Σ record_size + leafOverhead(N, cols) − prefix saving − Σ region saving
//
// and a region takes a non-plain kind exactly when its saving is positive.

// The leaf flags byte leads every leaf payload. Other bits are reserved.
export const LEAF_FLAG_FIXED_KEY = 1; // a u16 key length follows; no key directory
export const LEAF_FLAG_PREFIX = 2; // a u16 prefix length and the prefix follow; the key blob holds suffixes

// TAG_INLINE_BARE is the presence tag of an inline-plain text / bytea / json value in a leaf: the rest
// of the value's span is its content, with no u16 length.
export const TAG_INLINE_BARE = 0x05;

// Region kinds (the byte leading every stored column region). 3 and 5 are reserved.
export const REGION_PLAIN = 0;
export const REGION_FOR = 1; // base plus offsets
export const REGION_DICT = 2; // dictionary
export const REGION_ARR = 4; // packed integer arrays

// leafPrefixLen is the length of the prefix a leaf of n keys stores once: the common prefix of its
// first and last key (which every key between shares, keys being sorted) when storing it saves bytes
// (n·p > p + 2), else 0.
export function leafPrefixLen(n: number, first: Uint8Array, last: Uint8Array): number {
  if (n < 2) return 0;
  const max = Math.min(first.length, last.length, 0xffff);
  let p = 0;
  while (p < max && first[p] === last[p]) p++;
  if (n * p <= p + 2) return 0;
  return p;
}

// leafPrefixSaving is the bytes the shared prefix removes from a leaf of n keys: each key loses p
// bytes, and the page gains the u16 prefix length and the prefix.
export function leafPrefixSaving(n: number, first: Uint8Array, last: Uint8Array): number {
  const p = leafPrefixLen(n, first, last);
  return p === 0 ? 0 : n * p - p - 2;
}

// forWidth is the offset width of a base-plus-offsets region whose present values span d =
// largest − smallest, in a column of width w: the narrowest of 0, 1, 2, 4 bytes holding d, or w when
// none below w does.
export function forWidth(w: number, d: bigint): number {
  let bw = 8;
  if (d === 0n) bw = 0;
  else if (d < 0x100n) bw = 1;
  else if (d < 0x1_0000n) bw = 2;
  else if (d < 0x1_0000_0000n) bw = 4;
  return bw >= w ? w : bw;
}

// forWidthN is forWidth for a span held as a number (a column of width 4 or less).
export function forWidthN(w: number, d: number): number {
  let bw = 8;
  if (d === 0) bw = 0;
  else if (d < 0x100) bw = 1;
  else if (d < 0x1_0000) bw = 2;
  else if (d < 0x1_0000_0000) bw = 4;
  return bw >= w ? w : bw;
}

// forSaving is the bytes a column of width w saves as base plus offsets of width bw in a leaf of n
// records: 0 when the plain region is no larger.
export function forSaving(n: number, w: number, bw: number): number {
  const s = n * (w - bw) - (w + 1);
  return s > 0 ? s : 0;
}

// dictSaving is the bytes a dictionary saves in a region of n values with d entries and dup bytes of
// spans that repeat an earlier entry; 0 when the plain region is no larger or the entries exceed a
// one-byte code.
export function dictSaving(n: number, d: number, dup: number): number {
  if (d > 256) return 0;
  const s = n - 2 - 2 * d + dup;
  return s > 0 ? s : 0;
}

// bitLen is the bit length of a nonnegative value below 2^64 (0 for 0).
export function bitLen(d: bigint): number {
  let hi = Number(d >> 32n);
  if (hi !== 0) return 64 - Math.clz32(hi);
  hi = Number(d & 0xffff_ffffn);
  return hi === 0 ? 0 : 32 - Math.clz32(hi);
}

// bitLenN is the bit length of a nonnegative integer below 2^32 held as a number.
export function bitLenN(d: number): number {
  return d === 0 ? 0 : 32 - Math.clz32(d);
}

// arrSaving is the bytes the packed-array form saves in a region of n values of element width w:
// empties empty arrays, arrays other arrays holding elems elements packed at bits bits each.
export function arrSaving(
  n: number,
  w: number,
  empties: number,
  arrays: number,
  elems: number,
  bits: number,
): number {
  const plain = 2 * n + 3 * empties + 11 * arrays + elems * w;
  const packed = n + w + 1 + Math.ceil((elems * bits) / 8);
  return plain > packed ? plain - packed : 0;
}

// arrElems returns the offset of the element bytes of the inline-plain array body b[at, end) the
// packed form holds — one dimension, lower bound 1, no NULL element, 1 to 254 elements of width w —
// or 0 for the empty array (whose body has no elements), or -1 when the packed form cannot hold the
// value.
export function arrElems(b: Uint8Array, at: number, end: number, w: number): number {
  const len = end - at;
  if (len === 2 && b[at] === 0 && b[at + 1] === 0) return 0;
  if (len < 10 || b[at] !== 1 || b[at + 1] !== 0) return -1;
  const m = ((b[at + 2]! << 24) | (b[at + 3]! << 16) | (b[at + 4]! << 8) | b[at + 5]!) >>> 0;
  const lb = ((b[at + 6]! << 24) | (b[at + 7]! << 16) | (b[at + 8]! << 8) | b[at + 9]!) >>> 0;
  if (m < 1 || m > 254 || lb !== 1 || len !== 10 + m * w) return -1;
  return at + 10;
}

// beUint reads the w-byte big-endian unsigned integer at b[at].
export function beUint(b: Uint8Array, at: number, w: number): bigint {
  if (w <= 4) {
    let v = 0;
    for (let i = 0; i < w; i++) v = v * 256 + b[at + i]!;
    return BigInt(v);
  }
  let hi = 0;
  let lo = 0;
  for (let i = 0; i < w - 4; i++) hi = hi * 256 + b[at + i]!;
  for (let i = w - 4; i < w; i++) lo = lo * 256 + b[at + i]!;
  return (BigInt(hi) << 32n) | BigInt(lo);
}

// putBEUint writes v as a w-byte big-endian unsigned integer at b[at] (the low w bytes of v).
export function putBEUint(b: Uint8Array, at: number, w: number, v: bigint): void {
  if (w <= 4) {
    let x = Number(v & 0xffff_ffffn);
    for (let i = w - 1; i >= 0; i--) {
      b[at + i] = x & 0xff;
      x = Math.floor(x / 256);
    }
    return;
  }
  let lo = Number(v & 0xffff_ffffn);
  let hi = Number((v >> 32n) & 0xffff_ffffn);
  for (let i = w - 1; i >= w - 4; i--) {
    b[at + i] = lo & 0xff;
    lo = Math.floor(lo / 256);
  }
  for (let i = w - 5; i >= 0; i--) {
    b[at + i] = hi & 0xff;
    hi = Math.floor(hi / 256);
  }
}

// getBits reads bw bits (at most 64) at bit offset at, MSB-first.
export function getBits(b: Uint8Array, at: number, bw: number): bigint {
  if (bw <= 32) {
    let v = 0;
    for (let i = 0; i < bw; i++) {
      const p = at + i;
      v = v * 2 + ((b[p >> 3]! >> (7 - (p & 7))) & 1);
    }
    return BigInt(v);
  }
  const hiBits = bw - 32;
  return (getBits(b, at, hiBits) << 32n) | getBits(b, at + hiBits, 32);
}

// putBits writes the low bw bits of v at bit offset at, MSB-first, into zeroed bytes.
export function putBits(b: Uint8Array, at: number, bw: number, v: bigint): void {
  if (bw > 32) {
    putBits(b, at, bw - 32, v >> 32n);
    putBits(b, at + bw - 32, 32, v & 0xffff_ffffn);
    return;
  }
  const x = Number(v);
  for (let i = 0; i < bw; i++) {
    // Bit bw-1-i of x, without a 32-bit shift overflow.
    if (Math.floor(x / 2 ** (bw - 1 - i)) % 2 === 1) {
      const p = at + i;
      b[p >> 3] |= 0x80 >> (p & 7);
    }
  }
}

// beUintN reads the w-byte (w at most 4) big-endian unsigned integer at b[at] as a number.
export function beUintN(b: Uint8Array, at: number, w: number): number {
  let v = 0;
  for (let i = 0; i < w; i++) v = v * 256 + b[at + i]!;
  return v;
}

// putBEUintN writes the low w bytes (w at most 4) of the nonnegative integer x, big-endian, at b[at].
export function putBEUintN(b: Uint8Array, at: number, w: number, x: number): void {
  for (let i = w - 1; i >= 0; i--) {
    b[at + i] = x & 0xff;
    x = Math.floor(x / 256);
  }
}

// getBitsN is getBits for at most 32 bits, as a number.
export function getBitsN(b: Uint8Array, at: number, bw: number): number {
  let v = 0;
  for (let i = 0; i < bw; i++) {
    const p = at + i;
    v = v * 2 + ((b[p >> 3]! >> (7 - (p & 7))) & 1);
  }
  return v;
}

// putBitsN is putBits for at most 32 bits of a number below 2^32.
export function putBitsN(b: Uint8Array, at: number, bw: number, x: number): void {
  for (let i = bw - 1; i >= 0; i--) {
    if ((x & 1) === 1) {
      const p = at + i;
      b[p >> 3] |= 0x80 >> (p & 7);
    }
    x = Math.floor(x / 2);
  }
}
