// date_trunc and EXTRACT — the datetime field/truncation kernels (spec/design/timezones.md
// §9.1/§9.2). Pure functions over an instant's microseconds (or a wall-clock decomposition / an
// interval), shared by the executor's date_trunc / EXTRACT evaluation. Zone handling lives in the
// executor (it converts a timestamptz instant to a local wall-clock micros in the session/explicit
// zone, then calls these wall-clock kernels); this module is zone-free and so is a §8 cross-core
// determinism contract on its own — the same (unit/field, value) yields the byte-identical result on
// every core. Calendar math reuses timestamp's Hinnant core. The year-group fields and `year` use
// PostgreSQL's BC-aware year numbering (no year 0); jed stores astronomical years (0 = 1 BC), so
// toPgYear / fromPgYear convert at the boundary. The exact integer formulas are oracle-pinned.

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
function intervalQuarter(mo: bigint): bigint {
  if (mo === 0n) return 1n;
  const sign = mo < 0n ? -1n : 1n;
  const abs = mo < 0n ? -mo : mo;
  return sign * (abs / 3n + 1n);
}

const unrecognizedUnit = (u: string) =>
  engineError("invalid_parameter_value", `unit "${u}" not recognized`);

// dateTruncMicros truncates a wall-clock instant `micros` down to the start of `unit`. ±infinity
// passes through; an unrecognized unit is 22023.
export function dateTruncMicros(unit: string, micros: bigint): bigint {
  if (micros === POS_INFINITY || micros === NEG_INFINITY) return micros;
  const u = unit.toLowerCase();
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
      throw unrecognizedUnit(unit);
  }
}

// dateTruncInterval truncates an interval's fields down to `unit`. `week` is 0A000; an unrecognized
// unit is 22023.
export function dateTruncInterval(unit: string, iv: Interval): Interval {
  const u = unit.toLowerCase();
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
      throw engineError("feature_not_supported", 'unit "week" not supported for type interval');
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
      throw unrecognizedUnit(unit);
  }
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

// extractField returns EXTRACT(field FROM source) as numeric. An unsupported field for the type is
// 0A000; an unrecognized field is 22023; julian is a deferred field (0A000).
export function extractField(field: string, src: ExtractSrc): Decimal {
  const f = field.toLowerCase();
  switch (src.kind) {
    case "ts":
      return extractDatetime(f, src.micros, false, 0n, 0n);
    case "tstz":
      return extractDatetime(f, src.local, true, src.instant, src.offsetSecs);
    case "date":
      return extractDate(f, src.days);
    case "interval":
      return extractInterval(f, src.iv);
  }
}

function extractDatetime(
  field: string,
  micros: bigint,
  isTz: boolean,
  instant: bigint,
  offsetSecs: bigint,
): Decimal {
  if (micros === POS_INFINITY || micros === NEG_INFINITY) {
    // jed's decimal is finite-only (decimal.md §2); PG returns ±Infinity — a documented divergence
    // (timezones.md §9.2).
    throw engineError(
      "numeric_value_out_of_range",
      "cannot extract field from an infinite timestamp",
    );
  }
  const [y, mo, d, h, mi, s, us] = civilFromMicros(micros);
  const secUs = s * MICROS_PER_SEC + us;
  const days = daysFromCivil(y, mo, d);
  switch (field) {
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
      if (!isTz) {
        throw engineError(
          "feature_not_supported",
          `unit "${field}" not supported for type timestamp without time zone`,
        );
      }
      if (field === "timezone") return Decimal.fromBigInt(offsetSecs);
      if (field === "timezone_hour") return Decimal.fromBigInt(offsetSecs / 3600n);
      return Decimal.fromBigInt((offsetSecs % 3600n) / 60n);
    }
    case "julian":
      throw engineError("feature_not_supported", 'unit "julian" not supported yet (jed deferred)');
    default:
      throw engineError("invalid_parameter_value", `unit "${field}" not recognized`);
  }
}

function extractDate(field: string, days: bigint): Decimal {
  const [y, mo, d] = civilFromDays(days);
  switch (field) {
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
    case "microseconds":
    case "milliseconds":
    case "second":
    case "minute":
    case "hour":
    case "timezone":
    case "timezone_hour":
    case "timezone_minute":
    case "julian":
      throw engineError("feature_not_supported", `unit "${field}" not supported for type date`);
    default:
      throw engineError("invalid_parameter_value", `unit "${field}" not recognized`);
  }
}

function extractInterval(field: string, iv: Interval): Decimal {
  const months = BigInt(iv.months);
  const days = BigInt(iv.days);
  const micros = iv.micros;
  const years = months / 12n;
  const mo = months % 12n;
  const timeSecUs = micros % MICROS_PER_MIN;
  switch (field) {
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
      return Decimal.fromBigInt(intervalQuarter(mo));
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
    case "dow":
    case "isodow":
    case "doy":
    case "isoyear":
    case "julian":
    case "timezone":
    case "timezone_hour":
    case "timezone_minute":
      throw engineError("feature_not_supported", `unit "${field}" not supported for type interval`);
    default:
      throw engineError("invalid_parameter_value", `unit "${field}" not recognized`);
  }
}
