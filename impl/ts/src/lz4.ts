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

// The encoder writes into one block of the worst-case output size (every byte a literal: the input,
// one token, and its length-extension bytes), tracking its write position.
function maxCompressedLength(n: number): number {
  return n + Math.floor(n / 255) + 16;
}

// Length-extension bytes for a token nibble that hit 15: 255* then the remainder (lz4.md §1).
// Returns the next write position.
function emitLength(out: Uint8Array, o: number, n: number): number {
  while (n >= 255) {
    out[o++] = 255;
    n -= 255;
  }
  out[o++] = n;
  return o;
}

function emitSequence(
  out: Uint8Array,
  o: number,
  src: Uint8Array,
  litFrom: number,
  litTo: number,
  offset: number,
  mlen: number,
): number {
  const lit = litTo - litFrom;
  const ml = mlen - MIN_MATCH;
  out[o++] = (Math.min(lit, 15) << 4) | Math.min(ml, 15);
  if (lit >= 15) o = emitLength(out, o, lit - 15);
  out.set(src.subarray(litFrom, litTo), o);
  o += lit;
  // The u16 offset is LITTLE-endian — the one deliberate exception to the big-endian house
  // rule, so the blob stays readable by any conformant LZ4 decoder (lz4.md §1).
  out[o++] = offset & 0xff;
  out[o++] = (offset >> 8) & 0xff;
  if (ml >= 15) o = emitLength(out, o, ml - 15);
  return o;
}

function emitLastLiterals(out: Uint8Array, o: number, src: Uint8Array, litFrom: number): number {
  const lit = src.length - litFrom;
  out[o++] = Math.min(lit, 15) << 4;
  if (lit >= 15) o = emitLength(out, o, lit - 15);
  out.set(src.subarray(litFrom), o);
  return o + lit;
}

// lz4Compress is the pinned encoder (lz4.md §2): one input → one output, in every core.
export function lz4Compress(src: Uint8Array): Uint8Array {
  return lz4CompressScratch(src).slice();
}

// lz4CompressScratch is lz4Compress without the final copy: the block is a view of the worst-case
// output buffer, so a caller that keeps it copies it (the planner copies only a block it adopts).
export function lz4CompressScratch(src: Uint8Array): Uint8Array {
  const out = new Uint8Array(maxCompressedLength(src.length));
  let o = 0;
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
      o = emitSequence(out, o, src, anchor, p, p - cand, mlen);
      p += mlen; // positions inside the match are NOT hashed
      anchor = p;
    } else {
      p++; // step is always 1 (no acceleration)
    }
  }
  o = emitLastLiterals(out, o, src, anchor);
  return out.subarray(0, o);
}

function corrupt(msg: string): Error {
  return engineError("data_corrupted", msg);
}

// lz4Decompress decodes comp to exactly rawLen bytes or fails data_corrupted (lz4.md §3).
export function lz4Decompress(comp: Uint8Array, rawLen: number): Uint8Array {
  // No block expands more than 255x (a match-length extension byte adds at most 255 bytes), so this
  // is rawLen for any valid block and keeps a corrupt rawLen from sizing the buffer; a block that
  // would decode past it cannot reach rawLen and is caught below.
  const limit = Math.min(rawLen, 255 * comp.length + 16);
  const out = new Uint8Array(limit);
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
    if (o + lit > limit) throw corrupt("decompressed length overflow");
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
    if (o + ml > limit) throw corrupt("decompressed length overflow");
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
