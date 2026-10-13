// date_trunc and EXTRACT — the datetime field/truncation kernels (spec/design/timezones.md
// §9.1/§9.2). Pure functions over an instant's microseconds (or a wall-clock decomposition / an
// interval), shared by the executor's date_trunc / EXTRACT evaluation. Zone handling lives in the
// executor (it converts a timestamptz instant to a local wall-clock micros in the session/explicit
// zone, then calls these wall-clock kernels); this module is zone-free and so is a §8 cross-core
// determinism contract on its own — the same (unit/field, value) yields the byte-identical result on
// every core. Calendar math reuses timestamp's Hinnant core. The year-group fields and `year` use
// PostgreSQL's BC-aware year numbering (no year 0); jed stores astronomical years (0 = 1 BC), so
// toPgYear / fromPgYear convert at the boundary. The exact integer formulas are oracle-pinned.

import { DATE_NEG_INFINITY, DATE_POS_INFINITY } from "./date.ts";
import { DATE_TOKENS, DELTA_TOKENS, type DtToken, TOKEN_MAX_LEN } from "./datetime_units_gen.ts";
import { Decimal } from "./decimal.ts";
import { engineError } from "./errors.ts";
import type { Interval } from "./interval.ts";
import {
  civilFromDays,
  civilFromMicros,
  daysFromCivil,
  NEG_INFINITY,
  POS_INFINITY,
} from "./timestamp.ts";

const MICROS_PER_SEC = 1_000_000n;
const SECS_PER_DAY = 86_400n;
const MICROS_PER_DAY = SECS_PER_DAY * MICROS_PER_SEC;
const MICROS_PER_MIN = 60n * MICROS_PER_SEC;
const MICROS_PER_HOUR = 3_600n * MICROS_PER_SEC;
// 365.25 days/year * 86400 s/day = 31_557_600 s (integral — PG's interval-epoch year).
const SECS_PER_INTERVAL_YEAR = 31_557_600n;
const SECS_PER_INTERVAL_MONTH = 30n * SECS_PER_DAY;
// The Julian Day number of 1970-01-01 (PG's UNIX_EPOCH_JDATE).
const UNIX_EPOCH_JD = 2_440_588n;

function floorDiv(a: bigint, b: bigint): bigint {
  const q = a / b;
  if (a % b !== 0n && a < 0n !== b < 0n) return q - 1n;
  return q;
}
function floorMod(a: bigint, b: bigint): bigint {
  return a - floorDiv(a, b) * b;
}

// toPgYear converts an astronomical year (0 = 1 BC) to PostgreSQL's year numbering (no year 0;
// 1 BC = -1). EXTRACT and the year-group fields report this. astro <= 0 is BC.
function toPgYear(astro: bigint): bigint {
  return astro <= 0n ? astro - 1n : astro;
}
// fromPgYear is the inverse (a negative PG year is BC, shifted up by one to skip the missing year 0).
function fromPgYear(pg: bigint): bigint {
  return pg < 0n ? pg + 1n : pg;
}

// decimalScaled builds a numeric from an unscaled bigint and a scale (value = unscaled * 10^-scale).
function decimalScaled(unscaled: bigint, scale: number): Decimal {
  const neg = unscaled < 0n;
  return Decimal.fromDigitsScale(neg, (neg ? -unscaled : unscaled).toString(), scale);
}

// dowSun0 is the day of week, 0 = Sunday .. 6 = Saturday (PG `dow`). 1970-01-01 was a Thursday (=4).
function dowSun0(daysSinceEpoch: bigint): bigint {
  return floorMod(daysSinceEpoch + 4n, 7n);
}
// isodowMon1 is the ISO day of week, 1 = Monday .. 7 = Sunday (PG `isodow`).
function isodowMon1(daysSinceEpoch: bigint): bigint {
  return floorMod(daysSinceEpoch + 3n, 7n) + 1n;
}

function isoP(y: bigint): bigint {
  return floorMod(y + floorDiv(y, 4n) - floorDiv(y, 100n) + floorDiv(y, 400n), 7n);
}
function weeksInIsoYear(y: bigint): bigint {
  return isoP(y) === 4n || isoP(y - 1n) === 3n ? 53n : 52n;
}

// isoWeekYear returns [isoWeek, isoYearAstronomical] for a civil date (astronomical year).
function isoWeekYear(y: bigint, mo: bigint, d: bigint): [bigint, bigint] {
  const days = daysFromCivil(y, mo, d);
  const ordinal = days - daysFromCivil(y, 1n, 1n) + 1n;
  const weekday = isodowMon1(days);
  const week = floorDiv(ordinal - weekday + 10n, 7n);
  if (week < 1n) return [weeksInIsoYear(y - 1n), y - 1n];
  if (week > weeksInIsoYear(y)) return [1n, y + 1n];
  return [week, y];
}

function extractDecade(pgYear: bigint): bigint {
  return pgYear >= 0n ? pgYear / 10n : -((8n - pgYear) / 10n);
}
function extractCentury(pgYear: bigint): bigint {
  return pgYear >= 0n ? (pgYear + 99n) / 100n : -((99n - pgYear) / 100n);
}
function extractMillennium(pgYear: bigint): bigint {
  return pgYear >= 0n ? (pgYear + 999n) / 1000n : -((999n - pgYear) / 1000n);
}
// EXTRACT(quarter FROM interval) from the signed months field — PG's interval_part_common, which
// works from the months count rather than the truncated month-of-year so that a negative interval's
// quarter is the negation of the sign-reversed interval's: (months%12)/3+1 for months >= 0, else
// -((-months%12)/3+1) (so -12 mons is -1, not 1; oracle-pinned).
function intervalQuarter(months: bigint): bigint {
  if (months >= 0n) return (months % 12n) / 3n + 1n;
  return -((-months % 12n) / 3n + 1n);
}

// What a unit name means to EXTRACT / date_part (spec/design/timezones.md §9.6): a canonical unit,
// a reserved word other than `epoch` (`infinity`, `now`, …), or unknown.
type FieldName = { kind: "unit"; unit: string } | { kind: "reserved" } | { kind: "unknown" };

// The unit name as PostgreSQL's messages print it: ASCII-lowercased (downcase_truncate_identifier;
// a non-ASCII letter keeps its case).
export function lowerUnit(name: string): string {
  return name.replace(/[A-Z]/g, (c) => c.toLowerCase());
}

// PostgreSQL's datebsearch: the entry whose token equals the first TOKEN_MAX_LEN bytes of the
// lowercased name (so `microseconds` finds `microsecon`). Tokens are ASCII, so comparing the first
// TOKEN_MAX_LEN UTF-16 units is the same test.
function searchTokens(table: readonly DtToken[], name: string): DtToken | undefined {
  const key = name.slice(0, TOKEN_MAX_LEN);
  let lo = 0;
  let hi = table.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    const t = table[mid]!;
    if (t.token === key) return t;
    if (t.token < key) lo = mid + 1;
    else hi = mid;
  }
  return undefined;
}

// PostgreSQL's DecodeUnits over a lowercased name: the canonical unit a deltatktbl units entry
// names (`hrs` → `hour`). date_trunc reads unit names this way.
function decodeUnits(name: string): string | undefined {
  const t = searchTokens(DELTA_TOKENS, name);
  return t !== undefined && t.kind === "units" ? t.value : undefined;
}

// EXTRACT / date_part's field lookup over a lowercased name: PostgreSQL's DecodeUnits, then
// DecodeSpecial (datetktbl) when the name is not in deltatktbl at all.
function decodeField(name: string): FieldName {
  const dt = searchTokens(DELTA_TOKENS, name);
  if (dt !== undefined) {
    return dt.kind === "units" ? { kind: "unit", unit: dt.value } : { kind: "unknown" };
  }
  const t = searchTokens(DATE_TOKENS, name);
  if (t === undefined) return { kind: "unknown" };
  if (t.kind === "units") return { kind: "unit", unit: t.value };
  if (t.kind === "reserv" && t.value === "epoch") return { kind: "unit", unit: "epoch" };
  if (t.kind === "reserv") return { kind: "reserved" };
  return { kind: "unknown" };
}

// canonicalField is the canonical field an EXTRACT / date_part field name resolves to
// (`jd` → `julian`), or undefined when it names no field.
export function canonicalField(field: string): string | undefined {
  const f = decodeField(lowerUnit(field));
  return f.kind === "unit" ? f.unit : undefined;
}

const unitNotRecognized = (name: string, typeName: string) =>
  engineError("invalid_parameter_value", `unit "${name}" not recognized for type ${typeName}`);

const unitNotSupported = (name: string, typeName: string) =>
  engineError("feature_not_supported", `unit "${name}" not supported for type ${typeName}`);

// A timestamp family's type name, as PostgreSQL's unit messages print it.
const timestampTypeName = (tz: boolean): string =>
  tz ? "timestamp with time zone" : "timestamp without time zone";

// dateTruncMicros truncates a wall-clock instant `micros` down to the start of `unit` (`tz` names
// the source type in messages). The unit is resolved first, as PostgreSQL does: an unrecognized
// unit is 22023 and a timezone* unit 0A000, even over ±infinity, which otherwise passes through
// unchanged.
export function dateTruncMicros(unit: string, micros: bigint, tz: boolean): bigint {
  const name = lowerUnit(unit);
  const u = decodeUnits(name);
  if (u === undefined) throw unitNotRecognized(name, timestampTypeName(tz));
  if (micros === POS_INFINITY || micros === NEG_INFINITY) {
    // Validate the unit against a finite probe, then pass the infinity through.
    dateTruncMicros(unit, 0n, tz);
    return micros;
  }
  const [y, mo, d, h, mi, s, us] = civilFromMicros(micros);
  const rebuild = (
    y: bigint,
    mo: bigint,
    d: bigint,
    h: bigint,
    mi: bigint,
    s: bigint,
    us: bigint,
  ): bigint =>
    daysFromCivil(y, mo, d) * MICROS_PER_DAY +
    h * MICROS_PER_HOUR +
    mi * MICROS_PER_MIN +
    s * MICROS_PER_SEC +
    us;
  switch (u) {
    case "microseconds":
      return micros;
    case "milliseconds":
      return rebuild(y, mo, d, h, mi, s, (us / 1000n) * 1000n);
    case "second":
      return rebuild(y, mo, d, h, mi, s, 0n);
    case "minute":
      return rebuild(y, mo, d, h, mi, 0n, 0n);
    case "hour":
      return rebuild(y, mo, d, h, 0n, 0n, 0n);
    case "day":
      return rebuild(y, mo, d, 0n, 0n, 0n, 0n);
    case "week": {
      const days = daysFromCivil(y, mo, d);
      const monday0 = floorMod(days + 3n, 7n);
      return (days - monday0) * MICROS_PER_DAY;
    }
    case "month":
      return rebuild(y, mo, 1n, 0n, 0n, 0n, 0n);
    case "quarter":
      return rebuild(y, ((mo - 1n) / 3n) * 3n + 1n, 1n, 0n, 0n, 0n, 0n);
    case "year":
      return rebuild(y, 1n, 1n, 0n, 0n, 0n, 0n);
    case "decade": {
      const d10 = extractDecade(toPgYear(y));
      const startTm = d10 >= 1n ? d10 * 10n : d10 * 10n - 1n;
      return rebuild(fromPgYear(startTm), 1n, 1n, 0n, 0n, 0n, 0n);
    }
    case "century": {
      const c = extractCentury(toPgYear(y));
      const startTm = c >= 1n ? (c - 1n) * 100n + 1n : c * 100n;
      return rebuild(fromPgYear(startTm), 1n, 1n, 0n, 0n, 0n, 0n);
    }
    case "millennium": {
      const m = extractMillennium(toPgYear(y));
      const startTm = m >= 1n ? (m - 1n) * 1000n + 1n : m * 1000n;
      return rebuild(fromPgYear(startTm), 1n, 1n, 0n, 0n, 0n, 0n);
    }
    default:
      throw unitNotSupported(name, timestampTypeName(tz));
  }
}

// dateTruncInterval truncates an interval's fields down to `unit`. `week` and the timezone* units
// are 0A000; an unrecognized unit is 22023.
export function dateTruncInterval(unit: string, iv: Interval): Interval {
  const name = lowerUnit(unit);
  const u = decodeUnits(name);
  if (u === undefined) throw unitNotRecognized(name, "interval");
  const months = BigInt(iv.months);
  const micros = iv.micros;
  const keepMd = (m: bigint): Interval => ({ months: iv.months, days: iv.days, micros: m });
  const monthsOnly = (m: bigint): Interval => ({ months: Number(m), days: 0, micros: 0n });
  switch (u) {
    case "microseconds":
      return iv;
    case "milliseconds":
      return keepMd((micros / 1000n) * 1000n);
    case "second":
      return keepMd((micros / MICROS_PER_SEC) * MICROS_PER_SEC);
    case "minute":
      return keepMd((micros / MICROS_PER_MIN) * MICROS_PER_MIN);
    case "hour":
      return keepMd((micros / MICROS_PER_HOUR) * MICROS_PER_HOUR);
    case "day":
      return keepMd(0n);
    case "week":
      throw unitNotSupported(name, "interval").withDetail("Months usually have fractional weeks.");
    case "month":
      return monthsOnly(months);
    case "quarter":
      return monthsOnly((months / 3n) * 3n);
    case "year":
      return monthsOnly((months / 12n) * 12n);
    case "decade":
      return monthsOnly((months / 120n) * 120n);
    case "century":
      return monthsOnly((months / 1200n) * 1200n);
    case "millennium":
      return monthsOnly((months / 12000n) * 12000n);
    default:
      throw unitNotSupported(name, "interval");
  }
}

// julianParts splits wall-clock micros into the Julian Day number of its date (PG's date2j) and the
// microseconds into that day.
function julianParts(micros: bigint): [bigint, bigint] {
  const days = floorDiv(micros, MICROS_PER_DAY);
  return [days + UNIX_EPOCH_JD, micros - days * MICROS_PER_DAY];
}

// extractJulian is EXTRACT(julian FROM timestamp) over finite wall-clock micros: the Julian Day
// number plus the fraction of the day, exactly as PG's numeric path computes it — jd +
// (time-of-day micros / 86400000000), the division at numeric's select_div_scale.
function extractJulian(micros: bigint): Decimal {
  const [jd, todUs] = julianParts(micros);
  const frac = Decimal.fromBigInt(todUs).div(Decimal.fromBigInt(MICROS_PER_DAY));
  return Decimal.fromBigInt(jd).add(frac);
}

// datePartJulian is date_part('julian', timestamp) over finite wall-clock micros — PG's float8
// formula verbatim (jd + (seconds + fsec/1e6) / 86400.0, not the numeric value rounded), so the
// result is PG's to the bit. The caller handles an infinite source through extractPart.
export function datePartJulian(micros: bigint): number {
  const [jd, todUs] = julianParts(micros);
  const secs = todUs / MICROS_PER_SEC;
  const fsec = todUs % MICROS_PER_SEC;
  return Number(jd) + (Number(secs) + Number(fsec) / 1000000.0) / Number(SECS_PER_DAY);
}

// ExtractSrc is the source value of an EXTRACT(field FROM source). For a timestamptz the caller
// supplies the wall-clock `local` micros (already converted into the session zone), the raw
// `instant` (for `epoch`), and the zone `offsetSecs` (for the timezone* fields); for a timestamp
// only the wall micros.
export type ExtractSrc =
  | { kind: "ts"; micros: bigint }
  | { kind: "tstz"; instant: bigint; local: bigint; offsetSecs: bigint }
  | { kind: "date"; days: bigint }
  | { kind: "interval"; iv: Interval };

// ExtractOut is the result of the shared EXTRACT / date_part kernel: a finite value, or — over a
// ±infinity source — PostgreSQL's NonFiniteTimestampTzPart answer (timezones.md §9.2): NULL for an
// oscillating field (month, hour, dow, …), ±Infinity with the source's sign for a
// monotonically-increasing one (year, decade, century, millennium, julian, isoyear, epoch).
export type ExtractOut =
  | { kind: "value"; dec: Decimal }
  | { kind: "null" }
  | { kind: "infinite"; negative: boolean };

// extractField returns EXTRACT(field FROM source) as numeric, or null for NULL. The kernel's
// ±Infinity traps 22003: jed's decimal is finite-only (decimal.md §2), where PostgreSQL returns
// numeric ±Infinity — a documented divergence (timezones.md §9.2). An oscillating field over an
// infinite source is NULL, as in PG.
export function extractField(field: string, src: ExtractSrc): Decimal | null {
  const out = extractPart(field, src);
  if (out.kind === "value") return out.dec;
  if (out.kind === "null") return null;
  throw engineError(
    "numeric_value_out_of_range",
    `cannot extract field from an infinite ${src.kind === "date" ? "date" : "timestamp"}`,
  );
}

// isMonotonicField reports whether unit increases monotonically with time — PostgreSQL's
// NonFiniteTimestampTzPart split: these fields of a ±infinity source are ±Infinity; every other
// field it accepts oscillates and is NULL.
function isMonotonicField(unit: string): boolean {
  return (
    unit === "year" ||
    unit === "decade" ||
    unit === "century" ||
    unit === "millennium" ||
    unit === "julian" ||
    unit === "isoyear" ||
    unit === "epoch"
  );
}

// The ExtractOut of an infinite timestamp / timestamptz source (PG's NonFiniteTimestampTzPart: every
// unit is accepted, even timezone of a timestamp — oscillating, so NULL).
function nonFiniteTimestampPart(unit: string, micros: bigint): ExtractOut {
  if (isMonotonicField(unit)) return { kind: "infinite", negative: micros === NEG_INFINITY };
  return { kind: "null" };
}

// The ExtractOut of an infinite date (PG's extract_date): the monotonic fields are ±Infinity, the
// date's oscillating fields NULL, and every other field (the time and timezone fields) still 0A000.
function nonFiniteDatePart(unit: string, name: string, days: bigint): ExtractOut {
  switch (unit) {
    case "day":
    case "month":
    case "quarter":
    case "week":
    case "dow":
    case "isodow":
    case "doy":
      return { kind: "null" };
  }
  if (isMonotonicField(unit)) return { kind: "infinite", negative: days === DATE_NEG_INFINITY };
  throw unitNotSupported(name, "date");
}

// extractPart is the shared EXTRACT / date_part kernel. The field name is resolved first, through
// PostgreSQL's unit tables (so `hours`, `msec`, `jd` are hour, milliseconds, julian — timezones.md
// §9.6); then an unsupported field for the type is 0A000 and an unrecognized field 22023, both
// decided before an infinite source is (PG decodes the unit first).
export function extractPart(field: string, src: ExtractSrc): ExtractOut {
  const name = lowerUnit(field);
  const typeName =
    src.kind === "ts"
      ? timestampTypeName(false)
      : src.kind === "tstz"
        ? timestampTypeName(true)
        : src.kind;
  const f = decodeField(name);
  if (f.kind === "reserved" && src.kind !== "interval") {
    // A reserved word other than epoch is unsupported for the date/time types but unrecognized for
    // an interval — PostgreSQL's interval_part_common checks only epoch.
    throw unitNotSupported(name, typeName);
  }
  if (f.kind !== "unit") throw unitNotRecognized(name, typeName);
  switch (src.kind) {
    case "ts":
      if (src.micros === POS_INFINITY || src.micros === NEG_INFINITY) {
        return nonFiniteTimestampPart(f.unit, src.micros);
      }
      return { kind: "value", dec: extractDatetime(f.unit, name, src.micros, false, 0n, 0n) };
    case "tstz":
      if (src.local === POS_INFINITY || src.local === NEG_INFINITY) {
        return nonFiniteTimestampPart(f.unit, src.local);
      }
      return {
        kind: "value",
        dec: extractDatetime(f.unit, name, src.local, true, src.instant, src.offsetSecs),
      };
    case "date":
      if (src.days === DATE_POS_INFINITY || src.days === DATE_NEG_INFINITY) {
        return nonFiniteDatePart(f.unit, name, src.days);
      }
      return { kind: "value", dec: extractDate(f.unit, name, src.days) };
    case "interval":
      return { kind: "value", dec: extractInterval(f.unit, name, src.iv) };
  }
}

// extractDatetime extracts the canonical `unit` (written as `name`, for messages) from the finite
// wall-clock micros of a timestamp / timestamptz.
function extractDatetime(
  unit: string,
  name: string,
  micros: bigint,
  isTz: boolean,
  instant: bigint,
  offsetSecs: bigint,
): Decimal {
  const [y, mo, d, h, mi, s, us] = civilFromMicros(micros);
  const secUs = s * MICROS_PER_SEC + us;
  const days = daysFromCivil(y, mo, d);
  switch (unit) {
    case "microseconds":
      return decimalScaled(secUs, 0);
    case "milliseconds":
      return decimalScaled(secUs, 3);
    case "second":
      return decimalScaled(secUs, 6);
    case "minute":
      return Decimal.fromBigInt(mi);
    case "hour":
      return Decimal.fromBigInt(h);
    case "day":
      return Decimal.fromBigInt(d);
    case "month":
      return Decimal.fromBigInt(mo);
    case "quarter":
      return Decimal.fromBigInt((mo - 1n) / 3n + 1n);
    case "year":
      return Decimal.fromBigInt(toPgYear(y));
    case "decade":
      return Decimal.fromBigInt(extractDecade(toPgYear(y)));
    case "century":
      return Decimal.fromBigInt(extractCentury(toPgYear(y)));
    case "millennium":
      return Decimal.fromBigInt(extractMillennium(toPgYear(y)));
    case "week":
      return Decimal.fromBigInt(isoWeekYear(y, mo, d)[0]);
    case "dow":
      return Decimal.fromBigInt(dowSun0(days));
    case "isodow":
      return Decimal.fromBigInt(isodowMon1(days));
    case "doy":
      return Decimal.fromBigInt(days - daysFromCivil(y, 1n, 1n) + 1n);
    case "isoyear":
      return Decimal.fromBigInt(toPgYear(isoWeekYear(y, mo, d)[1]));
    case "epoch":
      return decimalScaled(isTz ? instant : micros, 6);
    case "timezone":
    case "timezone_hour":
    case "timezone_minute": {
      if (!isTz) throw unitNotSupported(name, timestampTypeName(false));
      if (unit === "timezone") return Decimal.fromBigInt(offsetSecs);
      if (unit === "timezone_hour") return Decimal.fromBigInt(offsetSecs / 3600n);
      return Decimal.fromBigInt((offsetSecs % 3600n) / 60n);
    }
    case "julian":
      return extractJulian(micros);
    default:
      throw unitNotSupported(name, timestampTypeName(isTz));
  }
}

// extractDate extracts the canonical `unit` (written as `name`, for messages) from a finite date.
function extractDate(unit: string, name: string, days: bigint): Decimal {
  const [y, mo, d] = civilFromDays(days);
  switch (unit) {
    case "day":
      return Decimal.fromBigInt(d);
    case "month":
      return Decimal.fromBigInt(mo);
    case "quarter":
      return Decimal.fromBigInt((mo - 1n) / 3n + 1n);
    case "year":
      return Decimal.fromBigInt(toPgYear(y));
    case "decade":
      return Decimal.fromBigInt(extractDecade(toPgYear(y)));
    case "century":
      return Decimal.fromBigInt(extractCentury(toPgYear(y)));
    case "millennium":
      return Decimal.fromBigInt(extractMillennium(toPgYear(y)));
    case "week":
      return Decimal.fromBigInt(isoWeekYear(y, mo, d)[0]);
    case "dow":
      return Decimal.fromBigInt(dowSun0(days));
    case "isodow":
      return Decimal.fromBigInt(isodowMon1(days));
    case "doy":
      return Decimal.fromBigInt(days - daysFromCivil(y, 1n, 1n) + 1n);
    case "isoyear":
      return Decimal.fromBigInt(toPgYear(isoWeekYear(y, mo, d)[1]));
    case "epoch":
      return Decimal.fromBigInt(days * SECS_PER_DAY);
    case "julian":
      return Decimal.fromBigInt(days + UNIX_EPOCH_JD);
    default:
      throw unitNotSupported(name, "date");
  }
}

// extractInterval extracts the canonical `unit` (written as `name`, for messages) from an interval.
function extractInterval(unit: string, name: string, iv: Interval): Decimal {
  const months = BigInt(iv.months);
  const days = BigInt(iv.days);
  const micros = iv.micros;
  const years = months / 12n;
  const mo = months % 12n;
  const timeSecUs = micros % MICROS_PER_MIN;
  switch (unit) {
    case "microseconds":
      return decimalScaled(timeSecUs, 0);
    case "milliseconds":
      return decimalScaled(timeSecUs, 3);
    case "second":
      return decimalScaled(timeSecUs, 6);
    case "minute":
      return Decimal.fromBigInt((micros / MICROS_PER_MIN) % 60n);
    case "hour":
      return Decimal.fromBigInt(micros / MICROS_PER_HOUR);
    case "day":
      return Decimal.fromBigInt(days);
    case "week":
      return Decimal.fromBigInt(days / 7n);
    case "month":
      return Decimal.fromBigInt(mo);
    case "quarter":
      return Decimal.fromBigInt(intervalQuarter(months));
    case "year":
      return Decimal.fromBigInt(years);
    case "decade":
      return Decimal.fromBigInt(years / 10n);
    case "century":
      return Decimal.fromBigInt(years / 100n);
    case "millennium":
      return Decimal.fromBigInt(years / 1000n);
    case "epoch": {
      const intSecs =
        years * SECS_PER_INTERVAL_YEAR + mo * SECS_PER_INTERVAL_MONTH + days * SECS_PER_DAY;
      return Decimal.fromBigInt(intSecs).add(decimalScaled(micros, 6));
    }
    default:
      throw unitNotSupported(name, "interval");
  }
}
