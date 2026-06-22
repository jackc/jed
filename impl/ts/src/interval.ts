// The interval type — value model, parsing, and rendering (spec/design/interval.md). A value is
// PostgreSQL's three independent fields: months (i32), days (i32), micros (i64). They are kept
// separate so `+ 1 month` is calendar-aware; comparison/ordering/dedup collapse them via the
// canonical 128-bit span (1 month = 30 days, 1 day = 24 h).
//
// This is a §8 determinism hotspot: the fractional-unit cascade, the half-away µs rounding, and
// the render format must be byte-identical across the Rust/Go/TS cores. ALL cascade/span math is
// `bigint` (JS `number` is f64 — loses i64 precision), matching Rust's i128 and Go's big.Int.

import { engineError, type EngineError } from "./errors.ts";
import {
  civilFromMicros,
  daysFromCivil,
  daysInMonth,
  NEG_INFINITY,
  POS_INFINITY,
} from "./timestamp.ts";

const MICROS_PER_SEC = 1_000_000n;
// Microseconds in one day — the canonical "1 day = 24 h" span weight.
export const MICROS_PER_DAY = 86_400n * MICROS_PER_SEC;
// Days in one month for the canonical span / fractional cascade (PG DAYS_PER_MONTH).
export const DAYS_PER_MONTH = 30n;
const MONTHS_PER_YEAR = 12n;
// Bounds (spec/design/interval.md) so the exact cascade stays well-defined; over-long → 22008.
const MAX_INT_DIGITS = 18;
const MAX_FRAC_DIGITS = 9;
const I32_MIN = -2147483648n;
const I32_MAX = 2147483647n;

// An interval value — three independent fields. months/days are i32-range integers, micros is the
// i64 µs offset (a bigint). Comparison/ordering/dedup go through the canonical span, NOT the
// field triple, so `'1 mon'` == `'30 days'` == `'720:00:00'`.
export interface Interval {
  months: number;
  days: number;
  micros: bigint;
}

// The canonical comparison key: a signed 128-bit microsecond span combining the three fields via
// 1 month = 30 days and 1 day = 24 h (PG interval_cmp_value). A bigint (exact, unbounded).
export function intervalSpan(iv: Interval): bigint {
  const days = BigInt(iv.months) * DAYS_PER_MONTH + BigInt(iv.days);
  return days * MICROS_PER_DAY + iv.micros;
}

// Compare two intervals by their canonical span: -1, 0, or 1.
export function intervalCmp(a: Interval, b: Interval): number {
  const sa = intervalSpan(a);
  const sb = intervalSpan(b);
  return sa < sb ? -1 : sa > sb ? 1 : 0;
}

// The order-preserving KEY body for an interval (method interval-span-i128,
// spec/design/encoding.md §2.10): the 16-byte order-preserving encoding of the canonical 128-bit
// span — int-be-signflip at i128 width (add the bias 2^127, emit a 16-byte big-endian unsigned
// integer), mapping the signed span range monotonically onto [0, 2^128) so negatives sort below
// positives. Fixed-width 16, so self-delimiting with no escape/terminator (like uuid). Because the
// key is the span, two field-distinct but span-equal intervals ('1 mon' / '30 days') produce
// identical bytes — a UNIQUE interval index treats them as one (the "equal but not identical"
// wrinkle, the decimal 1.5/1.50 precedent). A PK is NOT NULL, so the stored key is this bare
// 16-byte body. (micros is a bigint end-to-end, so the span is exact — CLAUDE.md §2.)
export function intervalEncodeKey(iv: Interval): Uint8Array {
  let v = intervalSpan(iv) + (1n << 127n);
  const out = new Uint8Array(16);
  for (let i = 15; i >= 0; i--) {
    out[i] = Number(v & 0xffn);
    v >>= 8n;
  }
  return out;
}

function checkedI32(v: number): number {
  if (v < -2147483648 || v > 2147483647) throw intervalFieldOverflow("interval out of range");
  return v;
}
function checkedI64(v: bigint): bigint {
  if (v < I64_MIN || v > I64_MAX) throw intervalFieldOverflow("interval out of range");
  return v;
}

// intervalAdd is field-wise interval addition (PG keeps the fields independent, no
// justification). An i32 month/day or i64 micros overflow traps 22008.
export function intervalAdd(a: Interval, b: Interval): Interval {
  return {
    months: checkedI32(a.months + b.months),
    days: checkedI32(a.days + b.days),
    micros: checkedI64(a.micros + b.micros),
  };
}

// intervalSub is field-wise interval subtraction. Overflow traps 22008.
export function intervalSub(a: Interval, b: Interval): Interval {
  return {
    months: checkedI32(a.months - b.months),
    days: checkedI32(a.days - b.days),
    micros: checkedI64(a.micros - b.micros),
  };
}

// makeInterval builds an interval from PostgreSQL make_interval's components (functions.md §11).
// years/months fold into the months field (×12), weeks/days into the days field (×7), and
// hours/mins plus the caller's pre-converted secMicros into the micros field — grouped
// (((hours*60)+mins)*60)*1e6 + secMicros like PG, checking i64 at each step like the other cores.
// All math here is exact bigint integer (the one float step, secs → secMicros, lives in the
// executor). Any i32 month/day or i64 micros overflow traps 22008.
export function makeInterval(
  years: bigint,
  months: bigint,
  weeks: bigint,
  days: bigint,
  hours: bigint,
  mins: bigint,
  secMicros: bigint,
): Interval {
  const monthsTotal = years * MONTHS_PER_YEAR + months;
  const daysTotal = weeks * 7n + days;
  const hm = checkedI64(hours * 60n + mins); // total minutes
  const sec = checkedI64(hm * 60n); // total seconds
  const micros = checkedI64(checkedI64(sec * MICROS_PER_SEC) + secMicros);
  if (
    monthsTotal < I32_MIN ||
    monthsTotal > I32_MAX ||
    daysTotal < I32_MIN ||
    daysTotal > I32_MAX
  ) {
    throw intervalFieldOverflow("interval out of range");
  }
  return { months: Number(monthsTotal), days: Number(daysTotal), micros };
}

// intervalNeg negates all three fields. i32::MIN / i64::MIN would overflow → 22008.
export function intervalNeg(a: Interval): Interval {
  return {
    months: checkedI32(-a.months),
    days: checkedI32(-a.days),
    micros: checkedI64(-a.micros),
  };
}

function bigGcd(a: bigint, b: bigint): bigint {
  a = a < 0n ? -a : a;
  b = b < 0n ? -b : b;
  while (b !== 0n) {
    [a, b] = [b, a % b];
  }
  return a;
}

// parseFactorDecimal parses a canonical decimal string `[-]int[.frac]` into an exact fraction
// [num, den] with den = 10^len(frac) (value = num/den). Caps the digit counts (matching the Rust
// i128 cascade's bound) so all cores trap at the same factor size; over-long → 22008.
export function parseFactorDecimal(s: string): [bigint, bigint] {
  const neg = s.startsWith("-");
  const body = neg ? s.slice(1) : s;
  const dot = body.indexOf(".");
  const intPart = dot >= 0 ? body.slice(0, dot) : body;
  const fracPart = dot >= 0 ? body.slice(dot + 1) : "";
  if (intPart.length > MAX_INT_DIGITS || fracPart.length > MAX_FRAC_DIGITS) {
    throw intervalFieldOverflow("interval factor has too many digits");
  }
  let num = BigInt(intPart + fracPart);
  if (neg) num = -num;
  const den = 10n ** BigInt(fracPart.length);
  return [num, den];
}

// mulByFraction is the exact ×÷ cascade (spec/design/interval.md §5): scale each field by
// fnum/fden (fden > 0), cascading the fractional part months→days→micros, µs rounded half away
// from zero. EXACT (bigint; mirrors Rust's i128). A field beyond i32/i64 traps 22008.
export function mulByFraction(iv: Interval, fnum: bigint, fden: bigint): Interval {
  let g = bigGcd(fnum, fden);
  if (g === 0n) g = 1n;
  const fn = fnum / g;
  const fd = fden / g;
  const m = BigInt(iv.months);
  const d = BigInt(iv.days);
  const u = iv.micros;

  const mTotal = m * fn;
  const rMonth = mTotal / fd; // trunc toward zero
  const fracMonth = mTotal - rMonth * fd;
  const mrd = fracMonth * DAYS_PER_MONTH;
  const mrdWhole = mrd / fd;
  const mrdFrac = mrd - mrdWhole * fd;

  const dTotal = d * fn;
  const rDayPart = dTotal / fd;
  const dayFrac = dTotal - rDayPart * fd;
  const rDay = rDayPart + mrdWhole;

  const timeNum = u * fn + (dayFrac + mrdFrac) * MICROS_PER_DAY;
  const rTime = roundDivBig(timeNum, fd);

  if (
    rMonth < I32_MIN ||
    rMonth > I32_MAX ||
    rDay < I32_MIN ||
    rDay > I32_MAX ||
    rTime < I64_MIN ||
    rTime > I64_MAX
  ) {
    throw intervalFieldOverflow("interval out of range");
  }
  return { months: Number(rMonth), days: Number(rDay), micros: rTime };
}

function floorDivB(a: bigint, b: bigint): bigint {
  let q = a / b;
  if (a % b !== 0n && a < 0n !== b < 0n) q -= 1n;
  return q;
}
function floorModB(a: bigint, b: bigint): bigint {
  let m = a % b;
  if (m !== 0n && m < 0n !== b < 0n) m += b;
  return m;
}

// tsShift computes ts + iv (or ts - iv with subtract) — the calendar-aware datetime arithmetic
// (spec/design/interval.md §5). Months added first WITH DAY-OF-MONTH CLAMPING (Jan 31 + 1 month
// -> Feb 28/29), then days (24 h each), then micros. ±infinity stays ±infinity; a result onto a
// sentinel or beyond the i64-µs range traps 22008.
export function tsShift(ts: bigint, iv: Interval, subtract: boolean): bigint {
  if (ts === NEG_INFINITY || ts === POS_INFINITY) return ts;
  const sign = subtract ? -1n : 1n;
  let t = ts;
  const months = sign * BigInt(iv.months);
  if (months !== 0n) {
    const [y, mo, d, h, mi, s, us] = civilFromMicros(t);
    const total = y * 12n + (mo - 1n) + months;
    const ny = floorDivB(total, 12n);
    const nmo = floorModB(total, 12n) + 1n;
    const maxd = daysInMonth(ny, nmo);
    const nd = d > maxd ? maxd : d;
    const days = daysFromCivil(ny, nmo, nd);
    t = (days * 86_400n + h * 3600n + mi * 60n + s) * MICROS_PER_SEC + us;
  }
  t = t + sign * BigInt(iv.days) * MICROS_PER_DAY;
  t = subtract ? t - iv.micros : t + iv.micros;
  // A finite instant must lie strictly between the ±infinity sentinels (= i64::MIN/MAX).
  if (t <= NEG_INFINITY || t >= POS_INFINITY) throw intervalFieldOverflow("timestamp out of range");
  return t;
}

// tsDiff computes a - b of two timestamps (or timestamptz) → an interval, justified into days +
// time with months = 0 (PG timestamp_mi → interval_justify_hours). An ±infinity operand traps
// 22008; a day count beyond i32 traps 22008.
export function tsDiff(a: bigint, b: bigint): Interval {
  if (a === NEG_INFINITY || a === POS_INFINITY || b === NEG_INFINITY || b === POS_INFINITY) {
    throw intervalFieldOverflow("cannot subtract infinite timestamps");
  }
  const micros = a - b;
  const days = micros / MICROS_PER_DAY; // trunc toward zero
  const rem = micros % MICROS_PER_DAY;
  if (days < -2147483648n || days > 2147483647n)
    throw intervalFieldOverflow("interval out of range");
  return { months: 0, days: Number(days), micros: rem };
}

// --- parsing -----------------------------------------------------------------

function invalidInterval(detail: string): EngineError {
  return engineError("invalid_datetime_format", detail);
}
function intervalFieldOverflow(detail: string): EngineError {
  return engineError("datetime_field_overflow", detail);
}

function isWs(b: number): boolean {
  return b === 0x20 || b === 0x09 || b === 0x0a || b === 0x0c || b === 0x0d;
}
function isDigit(b: number): boolean {
  return b >= 0x30 && b <= 0x39;
}
function isAlpha(b: number): boolean {
  return (b >= 0x41 && b <= 0x5a) || (b >= 0x61 && b <= 0x7a);
}

// roundDivBig rounds num/den to the nearest integer, half away from zero (the engine's one
// rounding mode). den > 0.
function roundDivBig(num: bigint, den: bigint): bigint {
  const q = num / den; // truncates toward zero
  const r = num - q * den;
  const twice = (r < 0n ? -r : r) * 2n;
  if (twice >= den) return num >= 0n ? q + 1n : q - 1n;
  return q;
}

interface Acc {
  months: bigint;
  days: bigint;
  micros: bigint;
}

const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;

function addChecked(a: bigint, b: bigint): bigint {
  const s = a + b;
  if (s < I64_MIN || s > I64_MAX) throw intervalFieldOverflow("interval out of range");
  return s;
}

// applyUnit adds value = sign * intPart.fracNum/fracDen of a unit to the accumulator, where the
// unit is measured in monthsPer, daysPer, or microsPer of one base field (exactly one nonzero).
// The integer part lands in that field; the fractional part cascades to the next-lower fields
// using 1 month = 30 days and 1 day = 24 h (spec/design/interval.md §3). All exact bigint math.
function applyUnit(
  acc: Acc,
  neg: boolean,
  intPart: bigint,
  fracNum: bigint,
  fracDen: bigint,
  monthsPer: bigint,
  daysPer: bigint,
  microsPer: bigint,
): void {
  let n = intPart * fracDen + fracNum;
  if (neg) n = -n;
  const d = fracDen;

  let months = 0n;
  let days = 0n;
  let microsNum = 0n;

  if (monthsPer !== 0n) {
    const total = n * monthsPer; // months * d
    months = total / d; // trunc toward zero
    const rem = total - months * d;
    const dayTotal = rem * DAYS_PER_MONTH;
    const wholeDays = dayTotal / d;
    days += wholeDays;
    const remDays = dayTotal - wholeDays * d;
    microsNum += remDays * MICROS_PER_DAY;
  }
  if (daysPer !== 0n) {
    const total = n * daysPer;
    const wholeDays = total / d;
    days += wholeDays;
    const remDays = total - wholeDays * d;
    microsNum += remDays * MICROS_PER_DAY;
  }
  if (microsPer !== 0n) {
    microsNum += n * microsPer;
  }

  if (months !== 0n) acc.months = addChecked(acc.months, months);
  if (days !== 0n) acc.days = addChecked(acc.days, days);
  if (microsNum !== 0n) acc.micros = addChecked(acc.micros, roundDivBig(microsNum, d));
}

// unitWeights returns the cascade weights [monthsPer, daysPer, microsPer] for a unit word
// (case-insensitive), or undefined for an unrecognized unit. Exactly one weight is nonzero.
function unitWeights(unit: string): [bigint, bigint, bigint] | undefined {
  switch (unit.toLowerCase()) {
    case "millennium":
    case "millennia":
    case "mil":
    case "mils":
      return [12000n, 0n, 0n];
    case "century":
    case "centuries":
    case "cent":
    case "c":
      return [1200n, 0n, 0n];
    case "decade":
    case "decades":
    case "dec":
    case "decs":
      return [120n, 0n, 0n];
    case "year":
    case "years":
    case "yr":
    case "yrs":
    case "y":
      return [MONTHS_PER_YEAR, 0n, 0n];
    case "month":
    case "months":
    case "mon":
    case "mons":
      return [1n, 0n, 0n];
    case "week":
    case "weeks":
    case "w":
      return [0n, 7n, 0n];
    case "day":
    case "days":
    case "d":
      return [0n, 1n, 0n];
    case "hour":
    case "hours":
    case "hr":
    case "hrs":
    case "h":
      return [0n, 0n, 3600n * MICROS_PER_SEC];
    case "minute":
    case "minutes":
    case "min":
    case "mins":
      return [0n, 0n, 60n * MICROS_PER_SEC];
    case "second":
    case "seconds":
    case "sec":
    case "secs":
    case "s":
      return [0n, 0n, MICROS_PER_SEC];
    case "millisecond":
    case "milliseconds":
    case "msec":
    case "msecs":
    case "ms":
      return [0n, 0n, 1000n];
    case "microsecond":
    case "microseconds":
    case "usec":
    case "usecs":
    case "us":
      return [0n, 0n, 1n];
    default:
      return undefined;
  }
}

class Cursor {
  b: Uint8Array;
  i = 0;
  constructor(s: string) {
    this.b = new TextEncoder().encode(s);
  }
  skipWs(): void {
    while (this.i < this.b.length && isWs(this.b[this.i])) this.i++;
  }
  done(): boolean {
    return this.i >= this.b.length;
  }
  peek(): number | undefined {
    return this.i < this.b.length ? this.b[this.i] : undefined;
  }
  // Read a run of ASCII digits as a bigint; undefined for an empty run (22007); more than
  // MAX_INT_DIGITS digits → 22008.
  readDigits(): bigint | undefined {
    const start = this.i;
    while (this.i < this.b.length && isDigit(this.b[this.i])) this.i++;
    if (this.i === start) return undefined;
    if (this.i - start > MAX_INT_DIGITS) {
      throw intervalFieldOverflow("interval field has too many digits");
    }
    return BigInt(new TextDecoder().decode(this.b.subarray(start, this.i)));
  }
  // Peek an ASCII-letter word (not consuming).
  peekWord(): string | undefined {
    const start = this.i;
    let j = start;
    while (j < this.b.length && isAlpha(this.b[j])) j++;
    if (j === start) return undefined;
    return new TextDecoder().decode(this.b.subarray(start, j));
  }
}

// Parse the time fields after an integer hour and a ':' — MM[:SS[.ffffff]] — adding their micros
// to acc with the given sign. hour is the already-read integer hour (unbounded; PG allows
// 100:00:00). Sub-µs digits round half away from zero (like timestamp).
function parseTime(acc: Acc, c: Cursor, neg: boolean, hour: bigint): void {
  c.i++; // consume ':'
  const minute = c.readDigits();
  if (minute === undefined) throw invalidInterval("expected minutes");
  let second = 0n;
  let fracUs = 0n;
  if (c.peek() === 0x3a) {
    c.i++;
    const s = c.readDigits();
    if (s === undefined) throw invalidInterval("expected seconds");
    second = s;
    if (c.peek() === 0x2e) {
      c.i++;
      fracUs = readFracUs(c);
    }
  }
  let total =
    hour * 3600n * MICROS_PER_SEC +
    minute * 60n * MICROS_PER_SEC +
    second * MICROS_PER_SEC +
    fracUs;
  if (neg) total = -total;
  acc.micros = addChecked(acc.micros, total);
}

// Read fractional-seconds digits after '.' into microseconds (0..1_000_000), 0–6 digits exact,
// 7+ rounded half away from zero — identical to the timestamp rule.
function readFracUs(c: Cursor): bigint {
  const start = c.i;
  while (c.i < c.b.length && isDigit(c.b[c.i])) c.i++;
  const digits = c.b.subarray(start, c.i);
  if (digits.length === 0) throw invalidInterval("expected fractional digits after '.'");
  let us = 0n;
  for (let k = 0; k < 6; k++) {
    us *= 10n;
    if (k < digits.length) us += BigInt(digits[k] - 0x30);
  }
  if (digits.length > 6 && digits[6] >= 0x35) us += 1n;
  return us;
}

// Read a unit value's fractional digits after '.' as [numerator, denominator] with the
// denominator a power of ten. More than MAX_FRAC_DIGITS digits → 22008.
function readUnitFrac(c: Cursor): [bigint, bigint] {
  const start = c.i;
  while (c.i < c.b.length && isDigit(c.b[c.i])) c.i++;
  const digits = c.b.subarray(start, c.i);
  if (digits.length === 0) throw invalidInterval("expected fractional digits after '.'");
  if (digits.length > MAX_FRAC_DIGITS) {
    throw intervalFieldOverflow("interval value has too many fractional digits");
  }
  let num = 0n;
  let den = 1n;
  for (const dd of digits) {
    num = num * 10n + BigInt(dd - 0x30);
    den *= 10n;
  }
  return [num, den];
}

// Parse an interval literal (the "unit + time" subset) into the three-field value. Errors:
// malformed syntax → 22007; a field beyond the representable range → 22008.
export function parseInterval(input: string): Interval {
  const c = new Cursor(input.trim());
  const acc: Acc = { months: 0n, days: 0n, micros: 0n };

  c.skipWs();
  if (c.peek() === 0x40) {
    c.i++; // an optional leading `@` (PG's verbose lead-in) is accepted and ignored
    c.skipWs();
  }
  if (c.done()) throw invalidInterval("empty interval");

  let ago = false;
  let sawField = false;
  while (!c.done()) {
    const word0 = c.peekWord();
    if (word0 !== undefined && word0.toLowerCase() === "ago") {
      c.i += word0.length;
      ago = true;
      c.skipWs();
      break;
    }

    let neg = false;
    const p = c.peek();
    if (p === 0x2d) {
      neg = true;
      c.i++;
    } else if (p === 0x2b) {
      c.i++;
    }
    const intPart = c.readDigits();
    if (intPart === undefined) throw invalidInterval("expected a number");

    if (c.peek() === 0x3a) {
      parseTime(acc, c, neg, intPart);
      sawField = true;
    } else {
      let fracNum = 0n;
      let fracDen = 1n;
      if (c.peek() === 0x2e) {
        c.i++;
        [fracNum, fracDen] = readUnitFrac(c);
      }
      c.skipWs();
      // A bare number with no unit defaults to SECONDS (PG); a trailing `ago` is left for the
      // loop top; a recognized unit applies its weights; else 22007.
      const secs: [bigint, bigint, bigint] = [0n, 0n, MICROS_PER_SEC];
      let weights: [bigint, bigint, bigint];
      const word = c.peekWord();
      if (word !== undefined && word.toLowerCase() === "ago") {
        weights = secs;
      } else if (word !== undefined) {
        c.i += word.length;
        const w = unitWeights(word);
        if (w === undefined) throw invalidInterval(`unknown interval unit "${word}"`);
        weights = w;
      } else {
        weights = secs;
      }
      applyUnit(acc, neg, intPart, fracNum, fracDen, weights[0], weights[1], weights[2]);
      sawField = true;
    }
    c.skipWs();
  }

  if (!c.done()) throw invalidInterval("trailing characters in interval");
  if (!sawField) throw invalidInterval("empty interval");

  if (ago) {
    acc.months = -acc.months;
    acc.days = -acc.days;
    acc.micros = addChecked(0n, -acc.micros);
  }

  if (acc.months < I32_MIN || acc.months > I32_MAX || acc.days < I32_MIN || acc.days > I32_MAX) {
    throw intervalFieldOverflow("interval out of range");
  }
  return { months: Number(acc.months), days: Number(acc.days), micros: acc.micros };
}

// --- rendering ---------------------------------------------------------------

function pad2(n: bigint): string {
  const s = n.toString();
  return s.length >= 2 ? s : "0" + s;
}

// Render an interval to PG's canonical `IntervalStyle = postgres` text (spec/design/interval.md
// §4). Pure integer→string formatting (no locale).
export function renderInterval(iv: Interval): string {
  const months = BigInt(iv.months);
  const days = BigInt(iv.days);
  const micros = iv.micros;
  if (months === 0n && days === 0n && micros === 0n) return "00:00:00";
  const year = months / MONTHS_PER_YEAR;
  const mon = months % MONTHS_PER_YEAR;

  let out = "";
  const state = { isZero: true, isBefore: false };
  out = addIntPart(out, year, "year", state);
  out = addIntPart(out, mon, "mon", state);
  out = addIntPart(out, days, "day", state);

  if (micros !== 0n || state.isZero) {
    const neg = micros < 0n;
    const a = neg ? -micros : micros;
    const h = a / (3600n * MICROS_PER_SEC); // unbounded hour (micros not justified into days)
    const mi = (a / (60n * MICROS_PER_SEC)) % 60n;
    const s = (a / MICROS_PER_SEC) % 60n;
    const us = a % MICROS_PER_SEC;
    if (!state.isZero) out += " ";
    out += neg ? "-" : state.isBefore ? "+" : "";
    out += `${pad2(h)}:${pad2(mi)}:${pad2(s)}`;
    if (us !== 0n) {
      const frac = us.toString().padStart(6, "0").replace(/0+$/, "");
      out += `.${frac}`;
    }
  }
  return out;
}

// Append one integer field (year/mon/day) in PG postgres-style: nothing when zero; otherwise a
// leading space (unless first), a `+` only when a previous field was negative and this one is
// positive, the value, the unit, and a plural `s` when the value is not exactly 1.
function addIntPart(
  out: string,
  value: bigint,
  unit: string,
  state: { isZero: boolean; isBefore: boolean },
): string {
  if (value === 0n) return out;
  if (!state.isZero) out += " ";
  if (state.isBefore && value > 0n) out += "+";
  out += `${value.toString()} ${unit}`;
  if (value !== 1n) out += "s";
  state.isBefore = value < 0n;
  state.isZero = false;
  return out;
}
