// UUID bit-level operations (spec/design/functions.md §12). Value<->text rendering/parsing
// lives in value.ts; this is the SEMANTIC bit work — extracting the version and embedded
// timestamp from the 16 raw big-endian bytes (byte 0 is the most-significant). All functions
// are PURE — deterministic functions of their input bytes. Computed over `bigint` (the i64
// discipline — JS numbers are f64; the v1 60-bit ticks and v7 ms*1000 exceed 2^53). The pure
// generator byte builders (buildUuidV4/buildUuidV7 below) live here too; the PRNG draws + clock
// for the generators live on the entropy+clock seam (seam.ts).

// 100-ns intervals between the Gregorian UUID epoch (1582-10-15 00:00:00 UTC) and the Unix
// epoch (1970-01-01 00:00:00 UTC) — the v1/v6 timestamp base (= 0x01B21DD213814000).
const GREGORIAN_OFFSET_100NS = 122_192_928_000_000_000n;

// isRfc4122 reports whether the value carries the RFC 4122 variant (top two bits of byte 8 are
// 10). Microsoft GUIDs (11), the legacy NCS variant (0), and the nil UUID (all zero) are not.
function isRfc4122(b: Uint8Array): boolean {
  return (b[8]! & 0xc0) === 0x80;
}

// beUint reads `len` big-endian bytes of `b` starting at `off` as a bigint.
function beUint(b: Uint8Array, off: number, len: number): bigint {
  let r = 0n;
  for (let i = 0; i < len; i++) r = (r << 8n) | BigInt(b[off + i]!);
  return r;
}

// uuidExtractVersion returns the version nibble (high nibble of byte 6), 0..15, for an RFC 4122
// UUID; null off-variant. Matches PostgreSQL 18 uuid_extract_version.
export function uuidExtractVersion(b: Uint8Array): bigint | null {
  if (!isRfc4122(b)) return null;
  return BigInt((b[6]! >> 4) & 0x0f);
}

// uuidExtractTimestampMicros returns the embedded instant as microseconds since the Unix epoch
// (a timestamptz), for an RFC 4122 UUID of VERSION 1 or 7 only; null otherwise. Matches
// PostgreSQL 18 uuid_extract_timestamp (v1/v7 only — v6 returns NULL there).
export function uuidExtractTimestampMicros(b: Uint8Array): bigint | null {
  if (!isRfc4122(b)) return null;
  switch ((b[6]! >> 4) & 0x0f) {
    case 7:
      return v7Micros(b);
    case 1:
      return v1Micros(b);
    default:
      return null;
  }
}

// v7: the first 6 bytes are a 48-bit big-endian Unix-millisecond count; micros = ms * 1000.
function v7Micros(b: Uint8Array): bigint {
  return beUint(b, 0, 6) * 1000n;
}

// v1: reassemble the 60-bit Gregorian 100-ns count from time_low (bytes 0..3), time_mid (bytes
// 4..5), and time_hi (the low 12 bits of bytes 6..7), subtract the 1582→1970 epoch offset, then
// truncate 100-ns ticks to microseconds (BigInt division truncates toward zero — PG drops the
// sub-microsecond remainder).
function v1Micros(b: Uint8Array): bigint {
  const timeLow = beUint(b, 0, 4);
  const timeMid = beUint(b, 4, 2);
  const timeHi = beUint(b, 6, 2) & 0x0fffn;
  const ticks = (timeHi << 48n) | (timeMid << 32n) | timeLow;
  return (ticks - GREGORIAN_OFFSET_100NS) / 10n;
}

// --- generator byte builders (spec/design/entropy.md §3) ---------------------
// Pure assembly of the 16 bytes from already-drawn random bytes (and, for v7, the timestamp +
// monotonic counter). The PRNG draws + clock resolution live on StmtRng (seam.ts).

// buildUuidV4 sets the version (4) and RFC 4122 variant over 16 random bytes (in place).
export function buildUuidV4(b: Uint8Array): Uint8Array {
  b[6] = (b[6]! & 0x0f) | 0x40; // version 4
  b[8] = (b[8]! & 0x3f) | 0x80; // RFC 4122 variant
  return b;
}

// buildUuidV7 assembles a 48-bit big-endian Unix-millisecond timestamp (bytes 0..5), a 12-bit
// monotonic counter in rand_a (bytes 6..7 low, RFC 9562 Method 1), and 8 rand_b bytes (8..15),
// with the version (7) and variant overwritten. `unixMs` is a bigint (exceeds 2^53); `counter` is
// a 12-bit number.
export function buildUuidV7(unixMs: bigint, counter: number, randB: Uint8Array): Uint8Array {
  const b = new Uint8Array(16);
  b[0] = Number((unixMs >> 40n) & 0xffn);
  b[1] = Number((unixMs >> 32n) & 0xffn);
  b[2] = Number((unixMs >> 24n) & 0xffn);
  b[3] = Number((unixMs >> 16n) & 0xffn);
  b[4] = Number((unixMs >> 8n) & 0xffn);
  b[5] = Number(unixMs & 0xffn);
  const randA = counter & 0x0fff;
  b[6] = 0x70 | ((randA >> 8) & 0x0f); // version 7 + rand_a high nibble
  b[7] = randA & 0xff; // rand_a low byte
  b.set(randB, 8);
  b[8] = (b[8]! & 0x3f) | 0x80; // RFC 4122 variant
  return b;
}
