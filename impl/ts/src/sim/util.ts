// Shared helpers for the TS deterministic-simulation runner (spec/design/simulation-testing.md):
// FNV-1a 64, the content-keyed decision function, UTF-8 byte-order string comparison, and Go-shaped
// text helpers, so the transcript matches the Go runner byte for byte. Test tooling only: nothing in
// src/lib.ts or the browser build imports src/sim/.

const MASK64 = 0xffff_ffff_ffff_ffffn;
const GAMMA = 0x9e3779b97f4a7c15n;
const MIX1 = 0xbf58476d1ce4e5b9n;
const MIX2 = 0x94d049bb133111ebn;

// mix is the splitmix64 finalizer (spec/encoding/prng.toml's mixer).
function mix(z: bigint): bigint {
  z = ((z ^ (z >> 30n)) * MIX1) & MASK64;
  z = ((z ^ (z >> 27n)) * MIX2) & MASK64;
  return z ^ (z >> 31n);
}

// decide is the content-keyed decision function (simulation-testing.md §6.2), all arithmetic mod 2^64.
export function decide(
  seed: bigint,
  lane: bigint,
  step: bigint,
  offset: bigint,
  version: bigint,
): bigint {
  let h = seed & MASK64;
  for (const w of [lane, step, offset, version]) h = mix(((h ^ w) + GAMMA) & MASK64);
  return h;
}

// Fnv64 is an incremental FNV-1a 64 hash kept as two 32-bit halves (BigInt per byte is too slow for
// whole-image hashes). The prime is 2^40 + 0x1b3, so h * prime = h * 0x1b3 + (h << 40).
export class Fnv64 {
  hi = 0xcbf29ce4;
  lo = 0x84222325;

  update(bytes: Uint8Array): void {
    let hi = this.hi;
    let lo = this.lo;
    for (let i = 0; i < bytes.length; i++) {
      lo = (lo ^ bytes[i]!) >>> 0;
      const loP = lo * 0x1b3;
      const carry = Math.floor(loP / 0x1_0000_0000);
      const hiP = hi * 0x1b3 + carry + ((lo << 8) >>> 0);
      lo = loP >>> 0;
      hi = hiP >>> 0;
    }
    this.hi = hi;
    this.lo = lo;
  }

  value(): bigint {
    return (BigInt(this.hi) << 32n) | BigInt(this.lo);
  }

  hex(): string {
    return this.hi.toString(16).padStart(8, "0") + this.lo.toString(16).padStart(8, "0");
  }
}

const encoder = new TextEncoder();

export function utf8(s: string): Uint8Array {
  return encoder.encode(s);
}

export function fnvHex(bytes: Uint8Array): string {
  const h = new Fnv64();
  h.update(bytes);
  return h.hex();
}

export function fnvStringHex(s: string): string {
  return fnvHex(utf8(s));
}

// compareUtf8 orders strings by their UTF-8 bytes, which is Unicode code point order — Go's
// sort.Strings. JavaScript's default comparison uses UTF-16 code units, which differs for astral
// characters against U+E000..U+FFFF.
export function compareUtf8(a: string, b: string): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    const x = a.charCodeAt(i);
    const y = b.charCodeAt(i);
    if (x === y) continue;
    const xs = x >= 0xd800 && x <= 0xdfff;
    const ys = y >= 0xd800 && y <= 0xdfff;
    if (xs !== ys) return xs ? 1 : -1; // a surrogate (astral) sorts after every BMP unit
    return x < y ? -1 : 1;
  }
  return a.length - b.length;
}

export function compareBytes(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i]! - b[i]!;
  return a.length - b.length;
}

export function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && compareBytes(a, b) === 0;
}

// isGoSpace is unicode.IsSpace, which strings.TrimSpace and strings.Fields use.
function isGoSpace(c: number): boolean {
  switch (c) {
    case 0x09:
    case 0x0a:
    case 0x0b:
    case 0x0c:
    case 0x0d:
    case 0x20:
    case 0x85:
    case 0xa0:
    case 0x1680:
    case 0x2028:
    case 0x2029:
    case 0x202f:
    case 0x205f:
    case 0x3000:
      return true;
  }
  return c >= 0x2000 && c <= 0x200a;
}

export function trimSpace(s: string): string {
  let start = 0;
  let end = s.length;
  while (start < end && isGoSpace(s.charCodeAt(start))) start++;
  while (end > start && isGoSpace(s.charCodeAt(end - 1))) end--;
  return s.slice(start, end);
}

export function fields(s: string): string[] {
  const out: string[] = [];
  let start = -1;
  for (let i = 0; i < s.length; i++) {
    if (isGoSpace(s.charCodeAt(i))) {
      if (start >= 0) out.push(s.slice(start, i));
      start = -1;
    } else if (start < 0) start = i;
  }
  if (start >= 0) out.push(s.slice(start));
  return out;
}

// atoi is strconv.Atoi with the error ignored: anything but an optionally signed decimal is 0.
export function atoi(s: string | undefined): number {
  if (s === undefined || !/^[+-]?[0-9]+$/.test(s)) return 0;
  const n = Number(s);
  return Number.isSafeInteger(n) ? n : 0;
}

// parseInt64 / parseUint64 are strconv.ParseInt / ParseUint (base 10, 64-bit) with the error
// ignored: 0 on any failure.
export function parseInt64(s: string | undefined): bigint {
  if (s === undefined || !/^[+-]?[0-9]+$/.test(s)) return 0n;
  const n = BigInt(s);
  return n < -(1n << 63n) || n >= 1n << 63n ? 0n : n;
}

export function parseUint64(s: string | undefined): bigint {
  if (s === undefined || !/^\+?[0-9]+$/.test(s)) return 0n;
  const n = BigInt(s);
  return n >= 1n << 64n ? 0n : n;
}

// goQuote is Go's %q / strconv.Quote for the identifiers, SQLSTATEs, and hex digests a transcript
// carries: `"` and `\` are escaped, control characters use Go's escapes, and printable characters
// (including non-ASCII) pass through.
export function goQuote(s: string): string {
  let out = '"';
  for (const ch of s) {
    const c = ch.codePointAt(0)!;
    switch (c) {
      case 0x22:
        out += '\\"';
        continue;
      case 0x5c:
        out += "\\\\";
        continue;
      case 0x07:
        out += "\\a";
        continue;
      case 0x08:
        out += "\\b";
        continue;
      case 0x0c:
        out += "\\f";
        continue;
      case 0x0a:
        out += "\\n";
        continue;
      case 0x0d:
        out += "\\r";
        continue;
      case 0x09:
        out += "\\t";
        continue;
      case 0x0b:
        out += "\\v";
        continue;
    }
    if (c < 0x20 || c === 0x7f) out += "\\x" + c.toString(16).padStart(2, "0");
    else out += ch;
  }
  return out + '"';
}
