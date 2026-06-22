// The timestamp / timestamptz calendar math, parsing, and rendering
// (spec/design/timestamp.md). Both types are an i64 count of microseconds since the Unix
// epoch (1970-01-01 00:00:00 UTC), proleptic Gregorian, no leap seconds.
//
// This is a §8 determinism hotspot: the civil↔instant conversion (Hinnant), the parse
// grammar, and the render format must be byte-identical across the Rust/Go/TS cores. ALL µs
// math is `bigint` — JS `number` loses precision past 2^53. The civil↔days path uses bigint
// `/` (which truncates toward zero, like Rust/Go) paired with the Hinnant -399/-146096
// adjustment; the instant↔civil decomposition uses the floorDiv/floorMod helpers below.

import { engineError, type EngineError } from "./errors.ts";

// NEG_INFINITY is the -infinity sentinel (the smallest i64; sorts before every finite instant).
export const NEG_INFINITY = -9223372036854775808n;
// POS_INFINITY is the +infinity sentinel (the largest i64; sorts after every finite instant).
export const POS_INFINITY = 9223372036854775807n;

const MICROS_PER_SEC = 1_000_000n;
const SECS_PER_DAY = 86_400n;

// floorDiv is bigint division rounding toward negative infinity (bigint `/` truncates).
function floorDiv(a: bigint, b: bigint): bigint {
  let q = a / b;
  if (a % b !== 0n && a < 0n !== b < 0n) q -= 1n;
  return q;
}

// floorMod is the modulo matching floorDiv (always in [0, b) for b > 0).
function floorMod(a: bigint, b: bigint): bigint {
  let m = a % b;
  if (m !== 0n && m < 0n !== b < 0n) m += b;
  return m;
}

function isLeap(y: bigint): boolean {
  return y % 4n === 0n && (y % 100n !== 0n || y % 400n === 0n);
}

export function daysInMonth(y: bigint, month: bigint): bigint {
  switch (month) {
    case 1n:
    case 3n:
    case 5n:
    case 7n:
    case 8n:
    case 10n:
    case 12n:
      return 31n;
    case 4n:
    case 6n:
    case 9n:
    case 11n:
      return 30n;
    case 2n:
      return isLeap(y) ? 29n : 28n;
    default:
      return 0n;
  }
}

// daysFromCivil returns days since 1970-01-01 for the civil date (y, m, d) (Hinnant). `y` is
// the astronomical year; `/` truncates, paired with the y-399 adjustment (= floor for y<0).
export function daysFromCivil(y: bigint, m: bigint, d: bigint): bigint {
  if (m <= 2n) y -= 1n;
  const era = (y >= 0n ? y : y - 399n) / 400n;
  const yoe = y - era * 400n;
  const doy = (153n * (m + (m > 2n ? -3n : 9n)) + 2n) / 5n + (d - 1n);
  const doe = yoe * 365n + yoe / 4n - yoe / 100n + doy;
  return era * 146097n + doe - 719468n;
}

// civilFromDays returns the civil date [year, month, day] from days since 1970-01-01.
export function civilFromDays(z: bigint): [bigint, bigint, bigint] {
  z += 719468n;
  const era = (z >= 0n ? z : z - 146096n) / 146097n;
  const doe = z - era * 146097n;
  const yoe = (doe - doe / 1460n + doe / 36524n - doe / 146096n) / 365n;
  const y = yoe + era * 400n;
  const doy = doe - (365n * yoe + yoe / 4n - yoe / 100n);
  const mp = (5n * doy + 2n) / 153n;
  const d = doy - (153n * mp + 2n) / 5n + 1n;
  const m = mp + (mp < 10n ? 3n : -9n);
  return [y + (m <= 2n ? 1n : 0n), m, d];
}

// civilFromMicros decomposes an instant into civil fields using FLOOR division (so pre-1970 /
// BC instants decompose correctly; us is always 0..999_999).
export function civilFromMicros(
  t: bigint,
): [bigint, bigint, bigint, bigint, bigint, bigint, bigint] {
  const us = floorMod(t, MICROS_PER_SEC);
  const secs = floorDiv(t, MICROS_PER_SEC);
  const sod = floorMod(secs, SECS_PER_DAY);
  const days = floorDiv(secs, SECS_PER_DAY);
  const [y, mo, d] = civilFromDays(days);
  return [y, mo, d, sod / 3600n, (sod % 3600n) / 60n, sod % 60n, us];
}

// --- parsing -----------------------------------------------------------------

export function invalidDatetime(detail: string): EngineError {
  return engineError("invalid_datetime_format", detail);
}

export function fieldOverflow(detail: string): EngineError {
  return engineError("datetime_field_overflow", detail);
}

function isWS(c: number): boolean {
  return c === 0x20 || c === 0x09 || c === 0x0a || c === 0x0c || c === 0x0d;
}

export function trimASCIIWS(s: string): string {
  let start = 0;
  let end = s.length;
  while (start < end && isWS(s.charCodeAt(start))) start++;
  while (end > start && isWS(s.charCodeAt(end - 1))) end--;
  return s.slice(start, end);
}

const MAX_I64 = 9223372036854775807n;

// A small mutable cursor (index into the string).
export type Cur = { i: number };

function isDigit(c: number): boolean {
  return c >= 48 && c <= 57;
}

// readUint reads one run of ASCII digits at cur.i as a bigint. Empty run → 22007; a value
// beyond i64 → 22008.
export function readUint(b: string, cur: Cur): bigint {
  const start = cur.i;
  let v = 0n;
  while (cur.i < b.length && isDigit(b.charCodeAt(cur.i))) {
    v = v * 10n + BigInt(b.charCodeAt(cur.i) - 48);
    if (v > MAX_I64) throw fieldOverflow("numeric field too large");
    cur.i++;
  }
  if (cur.i === start) throw invalidDatetime("expected a number");
  return v;
}

export function expectChar(b: string, cur: Cur, c: string): void {
  if (cur.i < b.length && b[cur.i] === c) {
    cur.i++;
    return;
  }
  throw invalidDatetime(`expected '${c}'`);
}

// readFrac parses fractional-seconds digits into microseconds (0..1_000_000; 1_000_000 means
// the rounding carried). 0–6 digits exact; 7+ round to µs half away from zero (7th digit >= 5).
export function readFrac(b: string, cur: Cur): bigint {
  const start = cur.i;
  while (cur.i < b.length && isDigit(b.charCodeAt(cur.i))) cur.i++;
  const digits = b.slice(start, cur.i);
  if (digits.length === 0) throw invalidDatetime("expected fractional digits after '.'");
  let us = 0n;
  for (let k = 0; k < 6; k++) {
    us *= 10n;
    if (k < digits.length) us += BigInt(digits.charCodeAt(k) - 48);
  }
  if (digits.length > 6 && digits.charCodeAt(6) >= 53 /* '5' */) us += 1n;
  return us;
}

function parseDatetime(input: string, applyOffset: boolean, typeName: string): bigint {
  const s = trimASCIIWS(input);
  const low = s.toLowerCase();

  if (low === "infinity" || low === "+infinity") return POS_INFINITY;
  if (low === "-infinity") return NEG_INFINITY;

  let bc = false;
  let body = s;
  if (low.endsWith(" bc")) {
    bc = true;
    body = trimASCIIWS(s.slice(0, s.length - 3));
  } else if (low.endsWith(" ad")) {
    body = trimASCIIWS(s.slice(0, s.length - 3));
  }

  const cur: Cur = { i: 0 };
  const year = readUint(body, cur);
  expectChar(body, cur, "-");
  const month = readUint(body, cur);
  expectChar(body, cur, "-");
  const day = readUint(body, cur);

  const bad = (): EngineError => invalidDatetime(`invalid input syntax for type ${typeName}`);

  let hour = 0n;
  let minute = 0n;
  let second = 0n;
  let micro = 0n;
  if (cur.i < body.length && (body[cur.i] === " " || body[cur.i] === "T" || body[cur.i] === "t")) {
    cur.i++;
    hour = readUint(body, cur);
    expectChar(body, cur, ":");
    minute = readUint(body, cur);
    if (cur.i < body.length && body[cur.i] === ":") {
      cur.i++;
      second = readUint(body, cur);
      if (cur.i < body.length && body[cur.i] === ".") {
        cur.i++;
        micro = readFrac(body, cur);
      }
    }
  }

  let offsetSecs = 0n;
  if (cur.i < body.length) {
    const ch = body[cur.i];
    if (ch === "Z" || ch === "z") {
      cur.i++;
    } else if (ch === "+" || ch === "-") {
      const sign = ch === "-" ? -1n : 1n;
      cur.i++;
      const oh = readUint(body, cur);
      let om = 0n;
      let os = 0n;
      if (cur.i < body.length && body[cur.i] === ":") {
        cur.i++;
        om = readUint(body, cur);
        if (cur.i < body.length && body[cur.i] === ":") {
          cur.i++;
          os = readUint(body, cur);
        }
      }
      if (oh > 15n || om > 59n || os > 59n) throw fieldOverflow("time zone offset out of range");
      offsetSecs = sign * (oh * 3600n + om * 60n + os);
    } else {
      throw bad();
    }
  }
  if (cur.i !== body.length) throw bad();

  if (year < 1n || year > 999_999n) throw fieldOverflow("year out of range");
  if (month < 1n || month > 12n) throw fieldOverflow("month out of range");
  const astro = bc ? 1n - year : year;
  if (day < 1n || day > daysInMonth(astro, month))
    throw fieldOverflow("day out of range for month");
  const extraDay = hour === 24n && minute === 0n && second === 0n && micro === 0n;
  if (hour > 23n && !extraDay) throw fieldOverflow("hour out of range");
  if (minute > 59n) throw fieldOverflow("minute out of range");
  if (second > 59n) throw fieldOverflow("second out of range");
  const hourPart = extraDay ? 0n : hour;

  let days = daysFromCivil(astro, month, day);
  if (extraDay) days += 1n;
  // bigint is arbitrary-precision; range-check against i64 explicitly after composing.
  let micros = days * SECS_PER_DAY + (hourPart * 3600n + minute * 60n + second);
  micros = micros * MICROS_PER_SEC + micro;
  if (applyOffset) micros -= offsetSecs * MICROS_PER_SEC;
  if (micros < NEG_INFINITY || micros > POS_INFINITY) throw fieldOverflow("value out of range");
  if (micros === NEG_INFINITY || micros === POS_INFINITY) throw fieldOverflow("value out of range");
  return micros;
}

// parseTimestamp parses a timestamp (zoneless) literal: an offset in the text is accepted and
// ignored (PG behavior).
export function parseTimestamp(s: string): bigint {
  return parseDatetime(s, false, "timestamp");
}

// parseTimestamptz parses a timestamptz literal: a trailing offset normalizes the value to UTC.
export function parseTimestamptz(s: string): bigint {
  return parseDatetime(s, true, "timestamptz");
}

// --- rendering ---------------------------------------------------------------

function pad(n: bigint, width: number): string {
  return n.toString().padStart(width, "0");
}

function renderDatetime(micros: bigint, isTz: boolean): string {
  if (micros === NEG_INFINITY) return "-infinity";
  if (micros === POS_INFINITY) return "infinity";
  const [y, mo, d, h, mi, s, us] = civilFromMicros(micros);
  const displayed = y <= 0n ? 1n - y : y;
  const era = y <= 0n ? " BC" : "";
  let out = `${pad(displayed, 4)}-${pad(mo, 2)}-${pad(d, 2)} ${pad(h, 2)}:${pad(mi, 2)}:${pad(s, 2)}`;
  if (us !== 0n) {
    const frac = pad(us, 6).replace(/0+$/, "");
    out += `.${frac}`;
  }
  if (isTz) out += "+00";
  out += era;
  return out;
}

// renderTimestamp renders a timestamp value to its canonical text.
export function renderTimestamp(micros: bigint): string {
  return renderDatetime(micros, false);
}

// renderTimestamptz renders a timestamptz value to its canonical text (always UTC, fixed +00).
export function renderTimestamptz(micros: bigint): string {
  return renderDatetime(micros, true);
}
