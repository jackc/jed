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
import { INTERVAL_QUALIFIERS, type IntervalQualifier } from "./datetime_units_gen.ts";
import type { TemporalTypmod } from "./types.ts";

const MICROS_PER_SEC = 1_000_000n;
// Microseconds in one day — the canonical "1 day = 24 h" span weight.
export const MICROS_PER_DAY = 86_400n * MICROS_PER_SEC;
// Days in one month for the canonical span / fractional cascade (PG DAYS_PER_MONTH).
export const DAYS_PER_MONTH = 30n;
const MONTHS_PER_YEAR = 12n;
// The digit bounds of an `interval × / number` factor (spec/design/interval.md §5), so the exact
// cascade stays well-defined; over-long → 22008.
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

// roundTiesEven rounds an f64 to the nearest integer, ties to EVEN (banker's rounding) — PG's
// `rint` / Rust's `round_ties_even` / Go's `math.RoundToEven`. JS `Math.round` rounds halves toward
// +Infinity, so a half-way value whose nearest-up integer is ODD is corrected one step down to the
// even neighbour (the down step is always −1 because Math.round took the tie up). For a non-tie
// value Math.round is already nearest; for a value too large to have a fractional part there is no
// tie. `-0` is preserved (Math.round(-0.5) = -0, and -0 is even so it is never corrected).
function roundTiesEven(x: number): number {
  const r = Math.round(x);
  if (Math.abs(x - Math.trunc(x)) === 0.5 && r % 2 !== 0) return r - 1;
  return r;
}

// intervalLerp(lo, hi, pct) = lo + (hi - lo)·pct, PG's orderedsetaggs.c interval interpolation
// (spec/design/aggregates.md §13). intervalMul below replicates PG's exact field-cascade + rounding
// so the result is byte-identical to PostgreSQL.
export function intervalLerp(lo: Interval, hi: Interval, pct: number): Interval {
  const diff = intervalSub(hi, lo);
  const scaled = intervalMul(diff, pct);
  return intervalAdd(scaled, lo);
}

// intervalMul is interval * f64, byte-identical to PostgreSQL's interval_mul (timestamp.c): multiply
// each field by the factor, then cascade the fractional month/day parts down to days/micros with
// PG's TSROUND (round to microsecond precision) and the 30 days/month, 86400 s/day conversions. The
// operand is finite (no infinite intervals here) and the factor is a finite fraction in [0,1]. The
// per-field arithmetic is f64 (JS number), matching PG/Rust/Go — only the FINAL micros is an i64
// bigint. A field beyond i32/i64 traps 22008.
export function intervalMul(span: Interval, factor: number): Interval {
  const DAYS_PER_MONTH_F = 30.0;
  const SECS_PER_DAY_F = 86400.0;
  const USECS_PER_SEC_F = 1_000_000.0;
  // TSROUND: round to microsecond precision (PG TS_PREC_INV = 1e6). PG rint = ties-to-EVEN.
  const tsround = (j: number): number => roundTiesEven(j * USECS_PER_SEC_F) / USECS_PER_SEC_F;
  const oor = () => intervalFieldOverflow("interval out of range");
  // FLOAT8_FITS_IN_INT32/64: x in [INT_MIN, -INT_MIN) — matches Rust's fits_i32/fits_i64. The
  // bounds are exact powers of two (written `2 ** 31` / `2 ** 63` so the i64 bound is an f64-exact
  // literal — `9223372036854775808` does not round-trip as a decimal literal).
  const fitsI32 = (x: number): boolean => x >= -(2 ** 31) && x < 2 ** 31;
  const fitsI64 = (x: number): boolean => x >= -(2 ** 63) && x < 2 ** 63;

  const origMonth = span.months;
  const origDay = span.days;

  let resultDouble = span.months * factor;
  if (Number.isNaN(resultDouble) || !fitsI32(resultDouble)) throw oor();
  const resultMonth = Math.trunc(resultDouble);

  resultDouble = span.days * factor;
  if (Number.isNaN(resultDouble) || !fitsI32(resultDouble)) throw oor();
  let resultDay = Math.trunc(resultDouble);

  // Cascade fractional months → days, fractional days → micros (PG's exact sequence).
  const monthRemainderDays = tsround((origMonth * factor - resultMonth) * DAYS_PER_MONTH_F);
  let secRemainder = tsround(
    (origDay * factor - resultDay + monthRemainderDays - Math.trunc(monthRemainderDays)) *
      SECS_PER_DAY_F,
  );
  // Might exceed a day from rounding / cascade — push whole days up.
  if (Math.abs(secRemainder) >= SECS_PER_DAY_F) {
    resultDay = checkedI32(resultDay + Math.trunc(secRemainder / SECS_PER_DAY_F));
    secRemainder -= Math.trunc(secRemainder / SECS_PER_DAY_F) * SECS_PER_DAY_F;
  }
  resultDay = checkedI32(resultDay + Math.trunc(monthRemainderDays));
  resultDouble = roundTiesEven(Number(span.micros) * factor + secRemainder * USECS_PER_SEC_F);
  if (Number.isNaN(resultDouble) || !fitsI64(resultDouble)) throw oor();
  return { months: resultMonth, days: resultDay, micros: BigInt(resultDouble) };
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

// justifyHours is PostgreSQL justify_hours (spec/design/interval.md §5.1): whole 24 h chunks of
// micros move into days, then days and micros are given the same sign (borrowing one day). months
// is untouched. A day count beyond i32 traps 22008.
export function justifyHours(iv: Interval): Interval {
  const wholeDay = iv.micros / MICROS_PER_DAY; // trunc toward zero
  let micros = iv.micros - wholeDay * MICROS_PER_DAY;
  let days = checkedI32(iv.days + Number(wholeDay));
  if (days > 0 && micros < 0n) {
    micros += MICROS_PER_DAY;
    days--;
  } else if (days < 0 && micros > 0n) {
    micros -= MICROS_PER_DAY;
    days++;
  }
  return { months: iv.months, days, micros };
}

// justifyDays is PostgreSQL justify_days (spec/design/interval.md §5.1): whole 30-day chunks of
// days move into months, then months and days are given the same sign (borrowing one month).
// micros is untouched. A month count beyond i32 traps 22008.
export function justifyDays(iv: Interval): Interval {
  const dpm = Number(DAYS_PER_MONTH);
  const wholeMonth = Math.trunc(iv.days / dpm);
  let days = iv.days - wholeMonth * dpm;
  let months = checkedI32(iv.months + wholeMonth);
  if (months > 0 && days < 0) {
    days += dpm;
    months--;
  } else if (months < 0 && days > 0) {
    days -= dpm;
    months++;
  }
  return { months, days, micros: iv.micros };
}

// justifyInterval is PostgreSQL justify_interval (spec/design/interval.md §5.1): both
// justifications, with 0 <= |micros| < 24 h, 0 <= |days| < 30, and all three fields made the same
// sign. Ported step for step from PG's interval_justify_interval, including its pre-justification
// of days (which keeps the day sum within i32). A month count beyond i32 traps 22008.
export function justifyInterval(iv: Interval): Interval {
  const dpm = Number(DAYS_PER_MONTH);
  let months = iv.months;
  let days = iv.days;
  let micros = iv.micros;
  // Pre-justify days when days and micros share a sign (it might prevent overflow).
  if ((days > 0 && micros > 0n) || (days < 0 && micros < 0n)) {
    const wholeMonth = Math.trunc(days / dpm);
    days -= wholeMonth * dpm;
    months = checkedI32(months + wholeMonth);
  }
  const wholeDay = micros / MICROS_PER_DAY; // trunc toward zero
  micros -= wholeDay * MICROS_PER_DAY;
  // Cannot overflow: |days| < 30 after the pre-justify, or days and wholeDay differ in sign.
  days += Number(wholeDay);
  const wholeMonth = Math.trunc(days / dpm);
  days -= wholeMonth * dpm;
  months = checkedI32(months + wholeMonth);
  if (months > 0 && (days < 0 || (days === 0 && micros < 0n))) {
    days += dpm;
    months--;
  } else if (months < 0 && (days > 0 || (days === 0 && micros > 0n))) {
    days -= dpm;
    months++;
  }
  if (days > 0 && micros < 0n) {
    micros += MICROS_PER_DAY;
    days--;
  } else if (days < 0 && micros > 0n) {
    micros -= MICROS_PER_DAY;
    days++;
  }
  return { months, days, micros };
}

// ageRejectInfinite is age(dt1, dt2)'s infinity rule (spec/design/interval.md §5.2). PostgreSQL
// returns an infinite interval for one infinite operand and raises 22008 "interval out of range"
// for the same infinity twice; jed has no infinite interval, so EVERY infinite operand is that
// 22008. Checked before any zone consultation, like PG's timestamp_age / timestamptz_age.
export function ageRejectInfinite(dt1: bigint, dt2: bigint): void {
  const inf = (t: bigint) => t === NEG_INFINITY || t === POS_INFINITY;
  if (inf(dt1) || inf(dt2)) throw intervalFieldOverflow("interval out of range");
}

// age is PostgreSQL age(dt1, dt2) (spec/design/interval.md §5.2), ported step for step from
// timestamp_age / timestamptz_age: the field-wise symbolic difference of the two wall clocks local1
// and local2 (a timestamp's own value; a timestamptz decomposed in the session zone), with the sign
// taken from comparing the values dt1 and dt2 themselves. Each field is subtracted, the result is
// flipped so the later value comes first, negative fields borrow from the next higher one (a day
// borrows the length of the EARLIER value's month), and the sign is restored. Both operands are
// finite (ageRejectInfinite ran first).
export function age(dt1: bigint, dt2: bigint, local1: bigint, local2: bigint): Interval {
  const [y1, mo1, d1, h1, mi1, s1, us1] = civilFromMicros(local1);
  const [y2, mo2, d2, h2, mi2, s2, us2] = civilFromMicros(local2);
  // Form the symbolic difference.
  let usec = us1 - us2;
  let sec = s1 - s2;
  let min = mi1 - mi2;
  let hour = h1 - h2;
  let mday = d1 - d2;
  let mon = mo1 - mo2;
  let year = y1 - y2;
  const flip = dt1 < dt2;
  if (flip) {
    usec = -usec;
    sec = -sec;
    min = -min;
    hour = -hour;
    mday = -mday;
    mon = -mon;
    year = -year;
  }
  // Propagate any negative fields into the next higher field.
  while (usec < 0n) {
    usec += MICROS_PER_SEC;
    sec--;
  }
  while (sec < 0n) {
    sec += 60n;
    min--;
  }
  while (min < 0n) {
    min += 60n;
    hour--;
  }
  while (hour < 0n) {
    hour += 24n;
    mday--;
  }
  while (mday < 0n) {
    mday += flip ? daysInMonth(y1, mo1) : daysInMonth(y2, mo2);
    mon--;
  }
  while (mon < 0n) {
    mon += MONTHS_PER_YEAR;
    year--;
  }
  // Recover the sign.
  if (flip) {
    usec = -usec;
    sec = -sec;
    min = -min;
    hour = -hour;
    mday = -mday;
    mon = -mon;
    year = -year;
  }
  // PG itm2interval: the months total must fit i32, the time total i64.
  const months = year * MONTHS_PER_YEAR + mon;
  if (months < I32_MIN || months > I32_MAX || mday < I32_MIN || mday > I32_MAX) {
    throw intervalFieldOverflow("interval out of range");
  }
  const micros = checkedI64(
    hour * 3600n * MICROS_PER_SEC + min * 60n * MICROS_PER_SEC + sec * MICROS_PER_SEC + usec,
  );
  return { months: Number(months), days: Number(mday), micros };
}

// --- parsing -----------------------------------------------------------------

function invalidInterval(detail: string): EngineError {
  return engineError("invalid_datetime_format", detail);
}
function intervalFieldOverflow(detail: string): EngineError {
  return engineError("datetime_field_overflow", detail);
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

const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;

// The interval text parser is a port of PostgreSQL 18's interval_in (datetime.c): the input is
// broken into fields (ParseDateTime), the fields are read right to left (DecodeInterval), and only
// if that finds the input malformed is it read as ISO 8601 (DecodeISO8601Interval). The one
// numeric difference (spec/design/interval.md §3): PG carries a field's fraction in a double; jed
// carries it exactly, as a signed numerator over 10^25 (FRAC_DEN), and rounds the µs half away
// from zero (the engine's one mode).

// PostgreSQL's datetime-decode failures, thrown as DtError: "bad" (DTERR_BAD_FORMAT, 22007) or
// "overflow" (DTERR_FIELD_OVERFLOW, PG's 22015; jed's datetime field overflow 22008).
class DtError {
  kind: "bad" | "overflow";
  constructor(kind: "bad" | "overflow") {
    this.kind = kind;
  }
}
const BAD = new DtError("bad");
const OVERFLOW = new DtError("overflow");

// A field's fractional part is numerator / 10^25, its digits past the 25th dropped (far below a
// microsecond for every unit).
const FRAC_DIGITS = 25;
const FRAC_DEN = 10n ** 25n;

const USECS_PER_SEC = MICROS_PER_SEC;
const USECS_PER_MINUTE = 60n * USECS_PER_SEC;
const USECS_PER_HOUR = 3600n * USECS_PER_SEC;

// PG's struct pg_itm_in: the fields an interval is assembled from. Years stay apart from months
// until the end (itmin2interval). All bigint, range-checked as PG's int32/int64.
interface ItmIn {
  usec: bigint;
  mday: bigint;
  mon: bigint;
  year: bigint;
}

// PG's DTK_* unit codes — the values DecodeInterval's `type` carries between fields.
const DTK_NUMBER = 0; // the type after `ago`: no number may precede it
const DTK_TZ = 4;
const DTK_EARLY = 9;
const DTK_LATE = 10;
const IGNORE_DTF = 8; // the initial type: the rightmost unitless number is seconds
const DTK_SECOND = 18;
const DTK_MINUTE = 19;
const DTK_HOUR = 20;
const DTK_DAY = 21;
const DTK_WEEK = 22;
const DTK_MONTH = 23;
const DTK_QUARTER = 24;
const DTK_YEAR = 25;
const DTK_DECADE = 26;
const DTK_CENTURY = 27;
const DTK_MILLENNIUM = 28;
const DTK_MILLISEC = 29;
const DTK_MICROSEC = 30;
const DTK_JULIAN = 31;
const DTK_DOW = 32;
const DTK_DOY = 33;
const DTK_ISOYEAR = 36;
const DTK_ISODOW = 37;

// The per-field masks of DecodeInterval (PG DTK_M): each field claims its bits, and a field whose
// bits overlap an already-read field's is a duplicate (22007). week and day are DIFFERENT fields,
// as are year/decade/century/millennium and second/millisecond/microsecond.
const FIELD_MONTH = 1 << 1;
const FIELD_YEAR = 1 << 2;
const FIELD_DAY = 1 << 3;
const FIELD_HOUR = 1 << 10;
const FIELD_MINUTE = 1 << 11;
const FIELD_SECOND = 1 << 12;
const FIELD_MILLISECOND = 1 << 13;
const FIELD_MICROSECOND = 1 << 14;
const FIELD_WEEK = 1 << 24;
const FIELD_DECADE = 1 << 25;
const FIELD_CENTURY = 1 << 26;
const FIELD_MILLENNIUM = 1 << 27;
// Seconds with a fractional part (PG DTK_ALL_SECS_M).
const FIELD_ALL_SECS = FIELD_SECOND | FIELD_MILLISECOND | FIELD_MICROSECOND;
// A time HH:MM[:SS[.ffffff]] (PG DTK_TIME_M).
const FIELD_TIME = FIELD_HOUR | FIELD_MINUTE | FIELD_ALL_SECS;
// PG DTK_DATE_M.
const FIELD_DATE = FIELD_YEAR | FIELD_MONTH | FIELD_DAY;

// PG's MAXDATEFIELDS and interval_in's 256-byte field buffer: more fields, or more field text, is
// malformed.
const MAX_DATE_FIELDS = 25;
const FIELD_BUF_LEN = 256;

// isspace in the C locale.
function isPgSpace(b: number): boolean {
  return b === 0x20 || b === 0x09 || b === 0x0a || b === 0x0b || b === 0x0c || b === 0x0d;
}
function isDigit(b: number): boolean {
  return b >= 0x30 && b <= 0x39;
}
function isAlpha(b: number): boolean {
  return (b >= 0x41 && b <= 0x5a) || (b >= 0x61 && b <= 0x7a);
}
function isAlnum(b: number): boolean {
  return isDigit(b) || isAlpha(b);
}
function isHex(b: number): boolean {
  return isDigit(b) || (b >= 0x41 && b <= 0x46) || (b >= 0x61 && b <= 0x66);
}
// ispunct in the C locale (ASCII punctuation).
function isPunct(b: number): boolean {
  return (
    (b >= 0x21 && b <= 0x2f) ||
    (b >= 0x3a && b <= 0x40) ||
    (b >= 0x5b && b <= 0x60) ||
    (b >= 0x7b && b <= 0x7e)
  );
}
function lower(b: number): number {
  return b >= 0x41 && b <= 0x5a ? b + 0x20 : b;
}

const CH_PLUS = 0x2b;
const CH_MINUS = 0x2d;
const CH_DOT = 0x2e;
const CH_SLASH = 0x2f;
const CH_COLON = 0x3a;
const CH_UNDERSCORE = 0x5f;

// The kind of one field (ParseDateTime's ftype): "number" — digits, possibly with one `.`
// fraction (5, 1.5, .5); "string" — a lowercased word (day, ago); "date" — digits with - / .
// separators, or a word run into digits (1-2, 5., p1d); "time" — digits then `:` (1:00:00.5);
// "tz" — a sign then digits, `:`, `.`, `-` (-1, +02:03, -1-2); "special" — a sign then a word
// (-infinity).
type FieldType = "number" | "string" | "date" | "time" | "tz" | "special";

interface DtField {
  ftype: FieldType;
  text: string;
}

// PostgreSQL's ParseDateTime, as interval_in uses it: split the input into fields. Whitespace
// separates fields; any other punctuation starting a field is a separator too; text is lowercased.
function parseDateTime(input: string): DtField[] {
  const at = (k: number): number => (k < input.length ? input.charCodeAt(k) : 0);
  const fields: DtField[] = [];
  // Bytes of PG's field buffer used by the finished fields (each field's text plus its NUL).
  let bufUsed = 0;
  let i = 0;
  while (i < input.length) {
    const c = at(i);
    if (isPgSpace(c)) {
      i++;
      continue;
    }
    if (fields.length >= MAX_DATE_FIELDS) throw BAD;
    let text = "";
    // PG's APPEND_CHAR: the buffer must keep room for the field's terminating NUL.
    const push = (ch: number): void => {
      if (bufUsed + text.length + 1 >= FIELD_BUF_LEN) throw BAD;
      text += String.fromCharCode(ch);
    };
    let ftype: FieldType;
    if (isDigit(c)) {
      while (isDigit(at(i))) push(at(i++));
      if (at(i) === CH_COLON) {
        // a time field
        ftype = "time";
        push(CH_COLON);
        i++;
        while (isDigit(at(i)) || at(i) === CH_COLON || at(i) === CH_DOT) push(at(i++));
      } else if (at(i) === CH_MINUS || at(i) === CH_SLASH || at(i) === CH_DOT) {
        // a date field (or a number with a fraction)
        const delim = at(i);
        push(delim);
        i++;
        if (isDigit(at(i))) {
          ftype = delim === CH_DOT ? "number" : "date";
          while (isDigit(at(i))) push(at(i++));
          // a second matching delimiter makes a three-field date
          if (at(i) === delim) {
            ftype = "date";
            push(delim);
            i++;
            while (isDigit(at(i)) || at(i) === delim) push(at(i++));
          }
        } else {
          ftype = "date";
          while (isAlnum(at(i)) || at(i) === delim) push(lower(at(i++)));
        }
      } else {
        ftype = "number";
      }
    } else if (c === CH_DOT) {
      // a leading decimal point: fractional seconds
      push(CH_DOT);
      i++;
      while (isDigit(at(i))) push(at(i++));
      ftype = "number";
    } else if (isAlpha(c)) {
      ftype = "string";
      while (isAlpha(at(i))) push(lower(at(i++)));
      // A word may run into a date or zone name — unless it is a known date keyword.
      let isDate = false;
      if (at(i) === CH_MINUS || at(i) === CH_SLASH || at(i) === CH_DOT) isDate = true;
      else if (at(i) === CH_PLUS || isDigit(at(i))) isDate = dateKeyword(text) === undefined;
      if (isDate) {
        ftype = "date";
        for (;;) {
          push(lower(at(i++)));
          const n = at(i);
          if (
            !(
              n === CH_PLUS ||
              n === CH_MINUS ||
              n === CH_SLASH ||
              n === CH_UNDERSCORE ||
              n === CH_DOT ||
              n === CH_COLON ||
              isAlnum(n)
            )
          ) {
            break;
          }
        }
      }
    } else if (c === CH_PLUS || c === CH_MINUS) {
      push(c);
      i++;
      // whitespace after the sign is skipped
      while (isPgSpace(at(i))) i++;
      if (isDigit(at(i))) {
        ftype = "tz";
        push(at(i++));
        while (isDigit(at(i)) || at(i) === CH_COLON || at(i) === CH_DOT || at(i) === CH_MINUS)
          push(at(i++));
      } else if (isAlpha(at(i))) {
        ftype = "special";
        while (isAlpha(at(i))) push(lower(at(i++)));
      } else {
        throw BAD;
      }
    } else if (isPunct(c)) {
      // other punctuation only separates fields
      i++;
      continue;
    } else {
      throw BAD;
    }
    bufUsed += text.length + 1;
    fields.push({ ftype, text });
  }
  return fields;
}

// What a word means to DecodeInterval: [kind, DTK value]. "ignore" — skipped (at, on); "units" —
// a unit, the DTK_* code it gives the number to its left; "ago"; "reserv" — a reserved word
// (infinity, epoch, …); "other" — a date keyword with no meaning in an interval (a month name, am,
// …).
type WordKind = "ignore" | "units" | "ago" | "reserv" | "other";

// PG's interval unit table (deltatktbl), then its date keyword table (datetktbl) — the lookup
// DecodeUnits then DecodeSpecial do. undefined is an unknown word. PG compares at most 10
// characters, so any word starting with a 10-letter entry (millennium, microsecon, millisecon) is
// that unit. jed's one difference: a bare `m` (PG's minute) is ambiguous with month and is
// rejected.
function intervalWord(w: string): [WordKind, number] | undefined {
  if (w === "m") return undefined;
  if (w.startsWith("millennium")) return ["units", DTK_MILLENNIUM];
  if (w.startsWith("microsecon")) return ["units", DTK_MICROSEC];
  if (w.startsWith("millisecon")) return ["units", DTK_MILLISEC];
  switch (w) {
    case "ago":
      return ["ago", 0];
    case "c":
    case "cent":
    case "centuries":
    case "century":
      return ["units", DTK_CENTURY];
    case "d":
    case "day":
    case "days":
      return ["units", DTK_DAY];
    case "dec":
    case "decade":
    case "decades":
    case "decs":
      return ["units", DTK_DECADE];
    case "h":
    case "hour":
    case "hours":
    case "hr":
    case "hrs":
      return ["units", DTK_HOUR];
    case "mil":
    case "millennia":
    case "millennium":
    case "mils":
      return ["units", DTK_MILLENNIUM];
    case "min":
    case "mins":
    case "minute":
    case "minutes":
      return ["units", DTK_MINUTE];
    case "mon":
    case "mons":
    case "month":
    case "months":
      return ["units", DTK_MONTH];
    case "ms":
    case "msec":
    case "msecond":
    case "mseconds":
    case "msecs":
      return ["units", DTK_MILLISEC];
    case "qtr":
    case "quarter":
      return ["units", DTK_QUARTER];
    case "s":
    case "sec":
    case "second":
    case "seconds":
    case "secs":
      return ["units", DTK_SECOND];
    case "timezone":
      return ["units", DTK_TZ];
    case "us":
    case "usec":
    case "usecond":
    case "useconds":
    case "usecs":
      return ["units", DTK_MICROSEC];
    case "w":
    case "week":
    case "weeks":
      return ["units", DTK_WEEK];
    case "y":
    case "year":
    case "years":
    case "yr":
    case "yrs":
      return ["units", DTK_YEAR];
  }
  return dateKeyword(w);
}

// The date keywords with no meaning in an interval (month and weekday names, am/pm, …).
const OTHER_DATE_KEYWORDS = new Set([
  "ad",
  "am",
  "apr",
  "april",
  "aug",
  "august",
  "bc",
  "dec",
  "december",
  "dst",
  "feb",
  "february",
  "fri",
  "friday",
  "jan",
  "january",
  "jul",
  "july",
  "jun",
  "june",
  "mar",
  "march",
  "may",
  "mon",
  "monday",
  "nov",
  "november",
  "oct",
  "october",
  "pm",
  "sat",
  "saturday",
  "sep",
  "sept",
  "september",
  "sun",
  "sunday",
  "t",
  "thu",
  "thur",
  "thurs",
  "thursday",
  "tue",
  "tues",
  "tuesday",
  "wed",
  "wednesday",
  "weds",
]);

// PG's date keyword table (datetktbl), as an interval reads it.
function dateKeyword(w: string): [WordKind, number] | undefined {
  switch (w) {
    case "at":
    case "on":
      return ["ignore", 0];
    case "infinity":
    case "+infinity":
      return ["reserv", DTK_LATE];
    case "-infinity":
      return ["reserv", DTK_EARLY];
    case "allballs":
    case "epoch":
    case "now":
    case "today":
    case "tomorrow":
    case "yesterday":
      return ["reserv", DTK_NUMBER];
    case "d":
      return ["units", DTK_DAY];
    case "dow":
      return ["units", DTK_DOW];
    case "doy":
      return ["units", DTK_DOY];
    case "h":
      return ["units", DTK_HOUR];
    case "isodow":
      return ["units", DTK_ISODOW];
    case "isoyear":
      return ["units", DTK_ISOYEAR];
    case "j":
    case "jd":
    case "julian":
      return ["units", DTK_JULIAN];
    case "m":
      return ["units", DTK_MONTH];
    case "mm":
      return ["units", DTK_MINUTE];
    case "s":
      return ["units", DTK_SECOND];
    case "y":
      return ["units", DTK_YEAR];
  }
  return OTHER_DATE_KEYWORDS.has(w) ? ["other", 0] : undefined;
}

// C strtol (base 10) as PG's strtoi64 uses it on a field: an optional sign then digits. Returns
// the value and the characters consumed — 0 consumed (value 0) when there are no digits. A value
// beyond int64 is ERANGE, a field overflow.
function strtoi64(s: string): [bigint, number] {
  let i = 0;
  let neg = false;
  if (s.length > 0 && (s.charCodeAt(0) === CH_MINUS || s.charCodeAt(0) === CH_PLUS)) {
    neg = s.charCodeAt(0) === CH_MINUS;
    i = 1;
  }
  const start = i;
  while (i < s.length && isDigit(s.charCodeAt(i))) i++;
  if (i === start) return [0n, 0];
  let v = BigInt(s.slice(start, i));
  if (neg) v = -v;
  if (v < I64_MIN || v > I64_MAX) throw OVERFLOW;
  return [v, i];
}

// PG's strtoint: strtol limited to int32 (beyond it is ERANGE, a field overflow).
function strtoint(s: string): [bigint, number] {
  const [v, n] = strtoi64(s);
  if (v < I32_MIN || v > I32_MAX) throw OVERFLOW;
  return [v, n];
}

// The first FRAC_DIGITS of a digit run as a fraction numerator over FRAC_DEN. A fraction whose
// nonzero digits all lie past the 25th stays nonzero (numerator 1) — too small to move any field,
// but still "a fraction" (it decides whether seconds claim the sub-second fields).
function fracFromDigits(digits: string): bigint {
  const num = BigInt(digits.slice(0, FRAC_DIGITS).padEnd(FRAC_DIGITS, "0"));
  if (num === 0n && /[1-9]/.test(digits)) return 1n;
  return num;
}

// PG's ParseFraction: s is `.` then digits (or nothing) and nothing else.
function parseFraction(s: string): bigint {
  const digits = s.slice(1);
  if (!/^[0-9]*$/.test(digits)) throw BAD;
  return fracFromDigits(digits);
}

// ParseFractionalSecond: a `.digits` fraction of a second to whole µs — 0–6 digits exact, 7+
// rounded half away from zero on the 7th digit (jed's one mode; PG rounds a double half to even).
function parseFractionalSecond(s: string): bigint {
  const digits = s.slice(1);
  if (!/^[0-9]*$/.test(digits)) throw BAD;
  let us = BigInt(digits.slice(0, 6).padEnd(6, "0"));
  if (digits.length > 6 && digits.charCodeAt(6) >= 0x35) us += 1n;
  return us;
}

// --- the Adjust* helpers of datetime.c: add a scaled value into the fields, failing on overflow ---

function i32(v: bigint): bigint {
  if (v < I32_MIN || v > I32_MAX) throw OVERFLOW;
  return v;
}
function i64(v: bigint): bigint {
  if (v < I64_MIN || v > I64_MAX) throw OVERFLOW;
  return v;
}

// AdjustFractMicroseconds: frac * scale µs, rounded half away from zero.
function adjustFractMicros(frac: bigint, scale: bigint, itm: ItmIn): void {
  if (frac === 0n) return;
  itm.usec = i64(itm.usec + roundDivBig(frac * scale, FRAC_DEN));
}

// AdjustFractDays: frac * scale days — whole days, then the remaining fraction of a day as µs.
function adjustFractDays(frac: bigint, scale: bigint, itm: ItmIn): void {
  if (frac === 0n) return;
  const total = frac * scale;
  const extraDays = total / FRAC_DEN; // truncate toward zero
  itm.mday = i32(itm.mday + extraDays);
  adjustFractMicros(total - extraDays * FRAC_DEN, MICROS_PER_DAY, itm);
}

// AdjustFractYears: frac * scale years as whole months, rounded half to even (PG's rint); the rest
// of a month is dropped, as in PG.
function adjustFractYears(frac: bigint, scale: bigint, itm: ItmIn): void {
  const total = frac * scale * MONTHS_PER_YEAR;
  let extra = total / FRAC_DEN;
  const rem = total - extra * FRAC_DEN;
  const twiceRem = (rem < 0n ? -rem : rem) * 2n;
  if (twiceRem > FRAC_DEN || (twiceRem === FRAC_DEN && extra % 2n !== 0n)) {
    extra += total < 0n ? -1n : 1n;
  }
  itm.mon = i32(itm.mon + extra);
}

// AdjustMicroseconds: (val + frac) * scale µs.
function adjustMicros(val: bigint, frac: bigint, scale: bigint, itm: ItmIn): void {
  itm.usec = i64(itm.usec + i64(val * scale));
  adjustFractMicros(frac, scale, itm);
}

// AdjustDays: val * scale days.
function adjustDays(val: bigint, scale: bigint, itm: ItmIn): void {
  itm.mday = i32(itm.mday + i32(i32(val) * scale));
}

// AdjustMonths: val months.
function adjustMonths(val: bigint, itm: ItmIn): void {
  itm.mon = i32(itm.mon + i32(val));
}

// AdjustYears: val * scale years.
function adjustYears(val: bigint, scale: bigint, itm: ItmIn): void {
  itm.year = i32(itm.year + i32(i32(val) * scale));
}

// PG's DecodeTimeForInterval (DecodeTimeCommon): a time field H:M, H:M:S[.f], or M:S.f (two parts
// with a fraction are minutes and seconds). The time REPLACES itm.usec, as in PG — so a fractional
// unit to the time's left (read earlier) loses its µs part. Under MINUTE TO SECOND (mmSS) a bare
// `a:b` is minutes:seconds (PG DecodeTimeCommon).
function decodeTimeForInterval(s: string, mmSS: boolean, itm: ItmIn): void {
  const h = strtoi64(s);
  let hour = h[0];
  let cp = s.slice(h[1]);
  if (cp.charCodeAt(0) !== CH_COLON) throw BAD;
  const m = strtoint(cp.slice(1));
  let minute = m[0];
  cp = cp.slice(1 + m[1]);
  let sec = 0n;
  let fsec = 0n;
  if (cp.length === 0) {
    if (mmSS) {
      // M:S under MINUTE TO SECOND
      if (hour < I32_MIN || hour > I32_MAX) throw OVERFLOW;
      sec = minute;
      minute = hour;
      hour = 0n;
    }
    // else H:M
  } else if (cp.charCodeAt(0) === CH_DOT) {
    // mm:ss.fff
    fsec = parseFractionalSecond(cp);
    if (hour < I32_MIN || hour > I32_MAX) throw OVERFLOW;
    sec = minute;
    minute = hour;
    hour = 0n;
  } else if (cp.charCodeAt(0) === CH_COLON) {
    const [s2, n3] = strtoint(cp.slice(1));
    sec = s2;
    cp = cp.slice(1 + n3);
    if (cp.length > 0) {
      if (cp.charCodeAt(0) !== CH_DOT) throw BAD;
      fsec = parseFractionalSecond(cp);
    }
  } else {
    throw BAD;
  }
  if (hour < 0n || minute < 0n || minute > 59n || sec < 0n || sec > 60n || fsec > USECS_PER_SEC) {
    throw OVERFLOW;
  }
  itm.usec = i64(
    i64(i64(i64(hour * USECS_PER_HOUR) + minute * USECS_PER_MINUTE) + sec * USECS_PER_SEC) + fsec,
  );
}

// PG's DecodeInterval (IntervalStyle postgres, no field qualifier): read the fields right to left,
// so a unit word is met before the number it applies to. A number with no unit takes the last unit
// read: seconds at the right end, then whatever unit was read last — except that after an hour or
// a time field the next unitless number is days ('1 1:00', '5 1 hour'). N-M is years-months.
// A field qualifier (`range`, its datetime_units.toml code, 0 for none) gives the rightmost unitless
// number its least field's unit ('5' DAY is 5 days) and makes `a:b` read as mm:ss under MINUTE TO
// SECOND (interval.md §6.1). Returns whether the input was `infinity` / `-infinity`.
function decodeInterval(fields: DtField[], range: number, itm: ItmIn): boolean {
  const qual = intervalQualifier(range);
  const mmSS = qual?.mmSS === true;
  let isBefore = false;
  let parsingUnitVal = false;
  let infinite = false;
  let fmask = 0;
  let ty = IGNORE_DTF;
  const nf = fields.length;
  for (let i = nf - 1; i >= 0; i--) {
    const f = fields[i];
    let tmask = 0;
    let ftype = f.ftype;
    if (ftype === "time") {
      decodeTimeForInterval(f.text, mmSS, itm);
      tmask = FIELD_TIME;
      ty = DTK_DAY;
      parsingUnitVal = false;
    } else if (ftype === "tz") {
      // A signed time (-1:00, +02:03) — else a signed number, read below.
      let isTime = false;
      if (f.text.slice(1).includes(":")) {
        try {
          decodeTimeForInterval(f.text.slice(1), mmSS, itm);
          isTime = true;
        } catch (e) {
          if (!(e instanceof DtError)) throw e;
        }
      }
      if (isTime) {
        if (f.text.charCodeAt(0) === CH_MINUS) itm.usec = i64(-itm.usec);
        tmask = FIELD_TIME;
        ty = DTK_DAY;
        parsingUnitVal = false;
      } else {
        ftype = "number";
      }
    }
    if (ftype === "number" || ftype === "date") {
      // the qualifier's least field decides what the rightmost number is
      if (ty === IGNORE_DTF) ty = leastFieldDtk(qual);
      const s = f.text;
      const parsed = strtoi64(s);
      let val = parsed[0];
      const cp = s.slice(parsed[1]);
      const negative = s.charCodeAt(0) === CH_MINUS;
      let fval = 0n;
      if (cp.length === 0) {
        // a whole number
      } else if (cp.charCodeAt(0) === CH_MINUS) {
        // SQL-standard years-months
        const ym = strtoint(cp.slice(1));
        let v2 = ym[0];
        if (v2 < 0n || v2 >= MONTHS_PER_YEAR) throw OVERFLOW;
        if (cp.length !== 1 + ym[1]) throw BAD;
        ty = DTK_MONTH;
        if (negative) v2 = -v2;
        val = i64(i64(val * MONTHS_PER_YEAR) + v2);
      } else if (cp.charCodeAt(0) === CH_DOT) {
        fval = parseFraction(cp);
        if (negative) fval = -fval;
      } else {
        throw BAD;
      }
      switch (ty) {
        case DTK_MICROSEC:
          adjustMicros(val, fval, 1n, itm);
          tmask = FIELD_MICROSECOND;
          break;
        case DTK_MILLISEC:
          adjustMicros(val, fval, 1000n, itm);
          tmask = FIELD_MILLISECOND;
          break;
        case DTK_SECOND:
          adjustMicros(val, fval, USECS_PER_SEC, itm);
          // fractional seconds claim the millisecond and microsecond fields too
          tmask = fval === 0n ? FIELD_SECOND : FIELD_ALL_SECS;
          break;
        case DTK_MINUTE:
          adjustMicros(val, fval, USECS_PER_MINUTE, itm);
          tmask = FIELD_MINUTE;
          break;
        case DTK_HOUR:
          adjustMicros(val, fval, USECS_PER_HOUR, itm);
          tmask = FIELD_HOUR;
          ty = DTK_DAY; // a unitless number left of an hour is days
          break;
        case DTK_DAY:
          adjustDays(val, 1n, itm);
          adjustFractMicros(fval, MICROS_PER_DAY, itm);
          tmask = FIELD_DAY;
          break;
        case DTK_WEEK:
          adjustDays(val, 7n, itm);
          adjustFractDays(fval, 7n, itm);
          tmask = FIELD_WEEK;
          break;
        case DTK_MONTH:
          adjustMonths(val, itm);
          adjustFractDays(fval, DAYS_PER_MONTH, itm);
          tmask = FIELD_MONTH;
          break;
        case DTK_YEAR:
        case DTK_DECADE:
        case DTK_CENTURY:
        case DTK_MILLENNIUM: {
          let scale = 1n;
          tmask = FIELD_YEAR;
          if (ty === DTK_DECADE) {
            scale = 10n;
            tmask = FIELD_DECADE;
          } else if (ty === DTK_CENTURY) {
            scale = 100n;
            tmask = FIELD_CENTURY;
          } else if (ty === DTK_MILLENNIUM) {
            scale = 1000n;
            tmask = FIELD_MILLENNIUM;
          }
          adjustYears(val, scale, itm);
          adjustFractYears(fval, scale, itm);
          break;
        }
        default:
          throw BAD;
      }
      parsingUnitVal = false;
    } else if (ftype === "string" || ftype === "special") {
      // two unit words in a row
      if (parsingUnitVal) throw BAD;
      const word = intervalWord(f.text);
      if (word === undefined) throw BAD;
      const [kind, value] = word;
      if (kind === "ignore") {
        // skipped — but, as in PG, it resets the unit a number to its left takes
        ty = IGNORE_DTF;
        continue;
      } else if (kind === "units") {
        ty = value;
        parsingUnitVal = true;
      } else if (kind === "ago") {
        // `ago` only as the last field
        if (i !== nf - 1) throw BAD;
        isBefore = true;
        ty = DTK_NUMBER;
      } else if (kind === "reserv") {
        tmask = FIELD_DATE | FIELD_TIME;
        // only infinity / -infinity, and nothing after it
        if ((value !== DTK_LATE && value !== DTK_EARLY) || i !== nf - 1) throw BAD;
        infinite = true;
      } else {
        throw BAD;
      }
    }
    if ((tmask & fmask) !== 0) throw BAD;
    fmask |= tmask;
  }
  // at least one field, and no unit left without its number
  if (fmask === 0 || parsingUnitVal) throw BAD;
  if (isBefore) {
    itm.usec = i64(-itm.usec);
    itm.mday = i32(-itm.mday);
    itm.mon = i32(-itm.mon);
    itm.year = i32(-itm.year);
  }
  return infinite;
}

// strtod's range limits, to 40 significant digits: a decimal at or above the overflow threshold
// (2^1024 − 2^970, ~1.8e308) rounds to infinity, and a nonzero one below the underflow threshold
// (2^-1022 − 2^-1076, ~2.2e-308) is too small for a normal double — both ERANGE, which PG reports
// as malformed input.
const STRTOD_OVERFLOW_DIGITS = "1797693134862315807937289714053034150799";
const STRTOD_UNDERFLOW_DIGITS = "2225073858507201259573821257020768020077";

// Compare a significand's leading 40 digits (zero-padded) with a 40-digit threshold.
function cmpLeadDigits(d: string, t: string): number {
  const lead = d.slice(0, t.length).padEnd(t.length, "0");
  return lead < t ? -1 : lead > t ? 1 : 0;
}

// The signed-exponent tail [eE|pP][+-]digits of a strtod number at s[pos.i]; only consumed when
// digits follow. The value saturates far beyond any meaningful exponent.
function readExponentPart(s: string, pos: { i: number }, marker: number): bigint {
  const at = (k: number): number => (k < s.length ? s.charCodeAt(k) : 0);
  if (lower(at(pos.i)) !== marker) return 0n;
  let j = pos.i + 1;
  let neg = false;
  if (at(j) === CH_PLUS || at(j) === CH_MINUS) {
    neg = at(j) === CH_MINUS;
    j++;
  }
  if (!isDigit(at(j))) return 0n;
  let e = 0n;
  while (isDigit(at(j))) {
    e = e * 10n + BigInt(at(j) - 0x30);
    if (e > 1_000_000_000_000n) e = 1_000_000_000_000n;
    j++;
  }
  pos.i = j;
  return neg ? -e : e;
}

// A decimal strtod number digits[.digits][e[+-]digits] at s[i..] (no sign) as [integer part,
// fraction numerator over FRAC_DEN, end], or the ParseISO8601Number error: a value strtod would
// report ERANGE for is malformed, one beyond ±1e15 a field overflow.
function scanDecimal(s: string, start: number): [bigint, bigint, number] {
  const pos = { i: start };
  const at = (k: number): number => (k < s.length ? s.charCodeAt(k) : 0);
  let digits = "";
  let fracCount = 0n;
  let sawDigit = false;
  while (isDigit(at(pos.i))) {
    digits += s[pos.i++];
    sawDigit = true;
  }
  if (at(pos.i) === CH_DOT && (sawDigit || isDigit(at(pos.i + 1)))) {
    pos.i++;
    while (isDigit(at(pos.i))) {
      digits += s[pos.i++];
      fracCount++;
      sawDigit = true;
    }
  }
  if (!sawDigit) throw BAD;
  // value = significand * 10^exp, the significand without leading or trailing zeros
  let exp = readExponentPart(s, pos, 0x65) - fracCount;
  let sig = digits.replace(/^0+/, "");
  if (sig === "") return [0n, 0n, pos.i];
  const trimmed = sig.replace(/0+$/, "");
  exp += BigInt(sig.length - trimmed.length);
  sig = trimmed;
  // the power of ten of the leading digit
  const lead = BigInt(sig.length - 1) + exp;
  if (lead > 308n || (lead === 308n && cmpLeadDigits(sig, STRTOD_OVERFLOW_DIGITS) > 0)) throw BAD;
  if (lead < -308n || (lead === -308n && cmpLeadDigits(sig, STRTOD_UNDERFLOW_DIGITS) <= 0))
    throw BAD;
  if (lead > 15n || (lead === 15n && sig !== "1")) throw OVERFLOW;
  if (exp >= 0n) return [BigInt(sig) * 10n ** exp, 0n, pos.i];
  // k digits are after the decimal point
  const k = Number(-exp);
  if (sig.length > k) {
    return [BigInt(sig.slice(0, sig.length - k)), fracFromDigits(sig.slice(sig.length - k)), pos.i];
  }
  const zeros = Math.min(k - sig.length, FRAC_DIGITS);
  return [0n, fracFromDigits("0".repeat(zeros) + sig), pos.i];
}

// A hexadecimal strtod number 0x hex[.hex][p[+-]digits] whose digits start at s[i..] — as
// scanDecimal, computed exactly from its bits (the fraction to 64 bits).
function scanHex(s: string, start: number): [bigint, bigint, number] {
  const pos = { i: start };
  const at = (k: number): number => (k < s.length ? s.charCodeAt(k) : 0);
  let bits = "";
  let fracHex = 0n;
  while (isHex(at(pos.i)))
    bits += Number.parseInt(s[pos.i++], 16)
      .toString(2)
      .padStart(4, "0");
  if (at(pos.i) === CH_DOT) {
    pos.i++;
    while (isHex(at(pos.i))) {
      bits += Number.parseInt(s[pos.i++], 16)
        .toString(2)
        .padStart(4, "0");
      fracHex++;
    }
  }
  // value = bits * 2^e2
  const e2 = readExponentPart(s, pos, 0x70) - 4n * fracHex;
  bits = bits.replace(/^0+/, "");
  if (bits === "") return [0n, 0n, pos.i];
  const n = BigInt(bits.length);
  // the power of two of the leading bit, and of the lowest set bit
  const lead = n - 1n + e2;
  const last = BigInt(bits.lastIndexOf("1"));
  const low = n - 1n - last + e2;
  // the leading 54 bits all ones: rounds up to the next power of two in 53-bit precision
  const roundsUp = bits.length >= 54 && !bits.slice(0, 54).includes("0");
  if (lead > 1023n || (lead === 1023n && roundsUp)) throw BAD;
  // ERANGE on underflow: below the normal range and not exact in the subnormal range
  const tiny = lead < -1022n && !(lead === -1023n && roundsUp);
  if (tiny && low < -1074n) throw BAD;
  if (lead >= 50n) throw OVERFLOW;
  const bitAt = (pow: bigint): bigint => {
    // the bit worth 2^pow
    const p = lead - pow;
    return p >= 0n && p < n && bits[Number(p)] === "1" ? 1n : 0n;
  };
  let ipart = 0n;
  for (let pow = lead; pow >= 0n; pow--) ipart = ipart * 2n + bitAt(pow);
  let f64bits = 0n;
  for (let q = 1n; q <= 64n; q++) f64bits = f64bits * 2n + bitAt(-q);
  if (ipart > 1_000_000_000_000_000n || (ipart === 1_000_000_000_000_000n && f64bits > 0n))
    throw OVERFLOW;
  // f64bits / 2^64 = f64bits * 5^25 / 2^39 / 10^25
  return [ipart, (f64bits * 298_023_223_876_953_125n) >> 39n, pos.i];
}

// PG's ParseISO8601Number: a strtod number at s[i..] (it must start with a digit, `-`, or `.`) as
// [integer part, fraction, end], both parts carrying the sign.
function parseIso8601Number(s: string, i: number): [bigint, bigint, number] {
  const at = (k: number): number => (k < s.length ? s.charCodeAt(k) : 0);
  const c = at(i);
  if (!(isDigit(c) || c === CH_MINUS || c === CH_DOT)) throw BAD;
  const neg = c === CH_MINUS;
  const j = neg ? i + 1 : i;
  // strtod's infinity / NaN spellings: a value out of range
  const head = s.slice(j, j + 3).toLowerCase();
  if (head === "inf" || head === "nan") throw OVERFLOW;
  const isHexNum =
    at(j) === 0x30 &&
    lower(at(j + 1)) === 0x78 &&
    (isHex(at(j + 2)) || (at(j + 2) === CH_DOT && isHex(at(j + 3))));
  const [ipart, frac, end] = isHexNum ? scanHex(s, j + 2) : scanDecimal(s, j);
  return neg ? [-ipart, -frac, end] : [ipart, frac, end];
}

// PG's ISO8601IntegerWidth: the integer digits of a number field, past a leading `-`.
function iso8601IntegerWidth(s: string): number {
  let i = s.charCodeAt(0) === CH_MINUS ? 1 : 0;
  const start = i;
  while (isDigit(s.charCodeAt(i))) i++;
  return i - start;
}

// PG's DecodeISO8601Interval: the ISO 8601 "format with designators" (P1Y2M3DT4H5M6S, P2W, any
// field fractional or negative) and the "alternative format" (P0001-02-03T04:05:06, basic
// P00010203T040506). The whole input, untrimmed, must be the value.
function decodeIso8601Interval(s: string, itm: ItmIn): void {
  itm.usec = 0n;
  itm.mday = 0n;
  itm.mon = 0n;
  itm.year = 0n;
  if (s.length < 2 || s[0] !== "P") throw BAD;
  let i = 1;
  let datepart = true;
  let havefield = false;
  while (i < s.length) {
    if (s[i] === "T") {
      // the start of the time part
      datepart = false;
      havefield = false;
      i++;
      continue;
    }
    const fieldstart = i;
    let [val, fval, end] = parseIso8601Number(s, i);
    i = end;
    const unit = i < s.length ? s[i] : "";
    i++;
    if (datepart) {
      if (unit === "Y") {
        adjustYears(val, 1n, itm);
        adjustFractYears(fval, 1n, itm);
      } else if (unit === "M") {
        adjustMonths(val, itm);
        adjustFractDays(fval, DAYS_PER_MONTH, itm);
      } else if (unit === "W") {
        adjustDays(val, 7n, itm);
        adjustFractDays(fval, 7n, itm);
      } else if (unit === "D") {
        adjustDays(val, 1n, itm);
        adjustFractMicros(fval, MICROS_PER_DAY, itm);
      } else if (unit === "T" || unit === "" || unit === "-") {
        if (unit !== "-" && iso8601IntegerWidth(s.slice(fieldstart)) === 8 && !havefield) {
          // alternative format, basic: YYYYMMDD
          adjustYears(val / 10000n, 1n, itm);
          adjustMonths((val / 100n) % 100n, itm);
          adjustDays(val % 100n, 1n, itm);
          adjustFractMicros(fval, MICROS_PER_DAY, itm);
          if (unit === "") return;
          datepart = false;
          havefield = false;
          continue;
        }
        // alternative format, extended: YYYY-MM-DD
        if (havefield) throw BAD;
        adjustYears(val, 1n, itm);
        adjustFractYears(fval, 1n, itm);
        if (unit === "") return;
        if (unit === "T") {
          datepart = false;
          havefield = false;
          continue;
        }
        [val, fval, end] = parseIso8601Number(s, i);
        i = end;
        adjustMonths(val, itm);
        adjustFractDays(fval, DAYS_PER_MONTH, itm);
        if (i >= s.length) return;
        if (s[i] === "T") {
          datepart = false;
          havefield = false;
          continue;
        }
        if (s[i] !== "-") throw BAD;
        i++;
        [val, fval, end] = parseIso8601Number(s, i);
        i = end;
        adjustDays(val, 1n, itm);
        adjustFractMicros(fval, MICROS_PER_DAY, itm);
        if (i >= s.length) return;
        if (s[i] === "T") {
          datepart = false;
          havefield = false;
          continue;
        }
        throw BAD;
      } else {
        throw BAD;
      }
    } else if (unit === "H") {
      adjustMicros(val, fval, USECS_PER_HOUR, itm);
    } else if (unit === "M") {
      adjustMicros(val, fval, USECS_PER_MINUTE, itm);
    } else if (unit === "S") {
      adjustMicros(val, fval, USECS_PER_SEC, itm);
    } else if (unit === "" || unit === ":") {
      if (unit === "" && iso8601IntegerWidth(s.slice(fieldstart)) === 6 && !havefield) {
        // alternative format, basic: HHMMSS
        adjustMicros(val / 10000n, 0n, USECS_PER_HOUR, itm);
        adjustMicros((val / 100n) % 100n, 0n, USECS_PER_MINUTE, itm);
        adjustMicros(val % 100n, 0n, USECS_PER_SEC, itm);
        adjustFractMicros(fval, 1n, itm);
        return;
      }
      // alternative format, extended: HH:MM:SS
      if (havefield) throw BAD;
      adjustMicros(val, fval, USECS_PER_HOUR, itm);
      if (unit === "") return;
      [val, fval, end] = parseIso8601Number(s, i);
      i = end;
      adjustMicros(val, fval, USECS_PER_MINUTE, itm);
      if (i >= s.length) return;
      if (s[i] !== ":") throw BAD;
      i++;
      [val, fval, end] = parseIso8601Number(s, i);
      i = end;
      adjustMicros(val, fval, USECS_PER_SEC, itm);
      if (i >= s.length) return;
      throw BAD;
    } else {
      throw BAD;
    }
    havefield = true;
  }
}

// Parse interval text into the three-field value — PostgreSQL's interval_in
// (spec/design/interval.md §3): the PostgreSQL / SQL-standard field syntax, else ISO 8601.
// Malformed input is 22007; a field beyond its range is 22008 (PG's 22015).
export function parseInterval(input: string): Interval {
  return parseIntervalRanged(input, 0);
}

// parseIntervalTypmod parses interval text for a target carrying a type modifier — PostgreSQL's
// interval_in with a typmod (interval.md §6.1): the qualifier steers the parse, then the value is
// cut to it and rounded to its precision (adjustIntervalForTypmod). null is plain parseInterval.
export function parseIntervalTypmod(input: string, typmod: TemporalTypmod | null): Interval {
  if (typmod === null) return parseInterval(input);
  return adjustIntervalForTypmod(parseIntervalRanged(input, typmod.fields), typmod);
}

// leastFieldDtk is the DTK unit a qualifier's least field gives the rightmost unitless number
// (seconds with no qualifier).
function leastFieldDtk(q: IntervalQualifier | null): number {
  switch (q?.least) {
    case "year":
      return DTK_YEAR;
    case "month":
      return DTK_MONTH;
    case "day":
      return DTK_DAY;
    case "hour":
      return DTK_HOUR;
    case "minute":
      return DTK_MINUTE;
    default:
      return DTK_SECOND;
  }
}

function parseIntervalRanged(input: string, range: number): Interval {
  const itm: ItmIn = { usec: 0n, mday: 0n, mon: 0n, year: 0n };
  let infinite = false;
  try {
    try {
      infinite = decodeInterval(parseDateTime(input), range, itm);
    } catch (e) {
      if (e !== BAD) throw e;
      decodeIso8601Interval(input, itm);
    }
  } catch (e) {
    if (e === OVERFLOW)
      throw intervalFieldOverflow(`interval field value out of range: "${input}"`);
    if (e === BAD) throw invalidInterval(`invalid input syntax for type interval: "${input}"`);
    throw e;
  }
  // (jed has no infinite interval, so `infinity` is not an interval value)
  if (infinite) throw invalidInterval(`invalid input syntax for type interval: "${input}"`);
  const months = itm.year * MONTHS_PER_YEAR + itm.mon;
  if (months < I32_MIN || months > I32_MAX) throw intervalFieldOverflow("interval out of range");
  return { months: Number(months), days: Number(itm.mday), micros: itm.usec };
}

// --- type modifiers (interval.md §6.1) ----------------------------------------

// MAX_INTERVAL_PRECISION is the largest interval precision (PostgreSQL's MAX_INTERVAL_PRECISION); a
// larger declared precision is reduced to it.
export const MAX_INTERVAL_PRECISION = 6;

// intervalQualifier is the interval field qualifier with stable `code`
// (spec/types/datetime_units.toml), or null for code 0 (no qualifier) or an unknown code.
export function intervalQualifier(code: number): IntervalQualifier | null {
  return INTERVAL_QUALIFIERS.find((q) => q.code === code) ?? null;
}

// intervalQualifierByName is the qualifier written as `name` ("day to second"), if it is one of the
// thirteen.
export function intervalQualifierByName(name: string): IntervalQualifier | null {
  return INTERVAL_QUALIFIERS.find((q) => q.name === name) ?? null;
}

// adjustIntervalForTypmod cuts an interval to a type modifier — PostgreSQL's
// AdjustIntervalForTypmod: the fields below the qualifier's least field go (YEAR keeps whole years
// of the months and zeroes days and time; MONTH zeroes days and time; DAY zeroes the time; HOUR and
// MINUTE truncate the time toward zero — higher fields are kept), then the time rounds half away
// from zero to the precision. A rounding that overflows is 22008.
export function adjustIntervalForTypmod(iv: Interval, typmod: TemporalTypmod): Interval {
  const out: Interval = { months: iv.months, days: iv.days, micros: iv.micros };
  switch (intervalQualifier(typmod.fields)?.least) {
    case "year":
      out.months = Math.trunc(out.months / 12) * 12;
      out.days = 0;
      out.micros = 0n;
      break;
    case "month":
      out.days = 0;
      out.micros = 0n;
      break;
    case "day":
      out.micros = 0n;
      break;
    case "hour":
      out.micros = (out.micros / USECS_PER_HOUR) * USECS_PER_HOUR;
      break;
    case "minute":
      out.micros = (out.micros / USECS_PER_MINUTE) * USECS_PER_MINUTE;
      break;
  }
  if (typmod.precision !== null && typmod.precision < MAX_INTERVAL_PRECISION) {
    const scale = 10n ** BigInt(MAX_INTERVAL_PRECISION - typmod.precision);
    const offset = scale / 2n;
    const shifted = out.micros >= 0n ? out.micros + offset : out.micros - offset;
    if (shifted < I64_MIN || shifted > I64_MAX)
      throw intervalFieldOverflow("interval out of range");
    out.micros = shifted - (shifted % scale);
  }
  return out;
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
