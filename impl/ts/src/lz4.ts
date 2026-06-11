// The pinned LZ4-block codec (spec/fileformat/lz4.md) — hand-rolled, deterministic, and
// byte-identical across every core (a library is inadmissible: encoders diverge — CLAUDE.md §14;
// spec/design/large-values.md §6). The encoder's free parameters are FIXED by lz4.md §2 (greedy
// match search, step 1, a 4096-entry single-candidate hash table, no backward extension); the
// output is pinned by spec/fileformat/lz4_vectors.toml and the compressed_table.jed golden. The
// decoder (lz4.md §3) is total and safe: every read is bounds-checked, the output never grows
// past the expected length, and malformed input is a structured data_corrupted (CLAUDE.md §13).

import { engineError } from "./errors.ts";

const MIN_MATCH = 4;
const MAX_OFFSET = 65535;
const MFLIMIT = 12; // no match may start after len-12 (the block format's end constraint)
const LAST_LITERALS = 5; // no match may extend past len-5
const HASH_LOG = 12;
const HASH_MUL = 2654435761;

// le32 reads 4 bytes little-endian; the hash multiply wraps modulo 2^32 (Math.imul) and the
// shift result is forced unsigned (>>>) — the places a careless JS port diverges (lz4.md §2).
function le32(src: Uint8Array, p: number): number {
  return (src[p]! | (src[p + 1]! << 8) | (src[p + 2]! << 16) | (src[p + 3]! << 24)) >>> 0;
}

function hash(v: number): number {
  return Math.imul(v, HASH_MUL) >>> (32 - HASH_LOG);
}

// Length-extension bytes for a token nibble that hit 15: 255* then the remainder (lz4.md §1).
function emitLength(out: number[], n: number): void {
  while (n >= 255) {
    out.push(255);
    n -= 255;
  }
  out.push(n);
}

function emitSequence(out: number[], src: Uint8Array, litFrom: number, litTo: number, offset: number, mlen: number): void {
  const lit = litTo - litFrom;
  const ml = mlen - MIN_MATCH;
  out.push((Math.min(lit, 15) << 4) | Math.min(ml, 15));
  if (lit >= 15) emitLength(out, lit - 15);
  for (let i = litFrom; i < litTo; i++) out.push(src[i]!);
  // The u16 offset is LITTLE-endian — the one deliberate exception to the big-endian house
  // rule, so the blob stays readable by any conformant LZ4 decoder (lz4.md §1).
  out.push(offset & 0xff, (offset >> 8) & 0xff);
  if (ml >= 15) emitLength(out, ml - 15);
}

function emitLastLiterals(out: number[], src: Uint8Array, litFrom: number): void {
  const lit = src.length - litFrom;
  out.push(Math.min(lit, 15) << 4);
  if (lit >= 15) emitLength(out, lit - 15);
  for (let i = litFrom; i < src.length; i++) out.push(src[i]!);
}

// lz4Compress is the pinned encoder (lz4.md §2): one input → one output, in every core.
export function lz4Compress(src: Uint8Array): Uint8Array {
  const out: number[] = [];
  const table = new Int32Array(1 << HASH_LOG).fill(-1);
  let anchor = 0;
  let p = 0;
  const limit = src.length - MFLIMIT; // last legal match start (may be negative)
  while (p <= limit) {
    const h = hash(le32(src, p));
    const cand = table[h]!;
    table[h] = p; // store AFTER reading the candidate
    if (cand >= 0 && p - cand <= MAX_OFFSET && le32(src, cand) === le32(src, p)) {
      const maxend = src.length - LAST_LITERALS;
      let mlen = MIN_MATCH;
      while (p + mlen < maxend && src[cand + mlen] === src[p + mlen]) mlen++;
      emitSequence(out, src, anchor, p, p - cand, mlen);
      p += mlen; // positions inside the match are NOT hashed
      anchor = p;
    } else {
      p++; // step is always 1 (no acceleration)
    }
  }
  emitLastLiterals(out, src, anchor);
  return Uint8Array.from(out);
}

function corrupt(msg: string): Error {
  return engineError("data_corrupted", msg);
}

// lz4Decompress decodes comp to exactly rawLen bytes or fails data_corrupted (lz4.md §3).
export function lz4Decompress(comp: Uint8Array, rawLen: number): Uint8Array {
  const out = new Uint8Array(rawLen);
  let o = 0; // bytes decoded so far
  let i = 0;
  const n = comp.length;
  for (;;) {
    if (i >= n) throw corrupt("truncated compressed block");
    const token = comp[i]!;
    i++;
    let lit = token >> 4;
    if (lit === 15) {
      for (;;) {
        if (i >= n) throw corrupt("truncated compressed block");
        const b = comp[i]!;
        i++;
        lit += b;
        if (b !== 255) break;
      }
    }
    if (i + lit > n) throw corrupt("truncated compressed block");
    if (o + lit > rawLen) throw corrupt("decompressed length overflow");
    out.set(comp.subarray(i, i + lit), o);
    o += lit;
    i += lit;
    if (i === n) break; // a literals-only tail ends the block
    if (i + 2 > n) throw corrupt("truncated compressed block");
    const offset = comp[i]! | (comp[i + 1]! << 8);
    i += 2;
    if (offset === 0 || offset > o) throw corrupt("invalid match offset");
    let ml = token & 0x0f;
    if (ml === 15) {
      for (;;) {
        if (i >= n) throw corrupt("truncated compressed block");
        const b = comp[i]!;
        i++;
        ml += b;
        if (b !== 255) break;
      }
    }
    ml += MIN_MATCH;
    if (o + ml > rawLen) throw corrupt("decompressed length overflow");
    const from = o - offset;
    for (let k = 0; k < ml; k++) {
      // Byte-by-byte ascending: an overlapping match replicates the run (lz4.md §3).
      out[o] = out[from + k]!;
      o++;
    }
  }
  if (o !== rawLen) throw corrupt("decompressed length mismatch");
  return out;
}
