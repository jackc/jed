//! `date_trunc` and `EXTRACT` — the datetime field/truncation kernels
//! (spec/design/timezones.md §9.1/§9.2). Pure functions over an instant's microseconds (or a
//! wall-clock decomposition / an interval), shared by the executor's `date_trunc` / `EXTRACT`
//! evaluation. Zone handling lives in the executor (it converts a `timestamptz` instant to a
//! local wall-clock micros in the session/explicit zone, then calls these wall-clock kernels);
//! this module is zone-free and so is a §8 cross-core determinism contract on its own — the same
//! `(unit/field, value)` yields the byte-identical result on every core.
//!
//! Calendar math reuses `timestamp`'s Hinnant core (`civil_from_micros` / `days_from_civil` /
//! `civil_from_days`). The year-group fields (decade/century/millennium) and `year` use
//! PostgreSQL's BC-aware year numbering (no year 0): jed stores astronomical years (0 = 1 BC), so
//! [`to_pg_year`] / [`from_pg_year`] convert at the boundary. The exact integer formulas (incl. the
//! quirky BC cases) are oracle-pinned against `postgres:18`.

use crate::datetime_units_gen::{DATE_TOKENS, DELTA_TOKENS, DtToken, TOKEN_MAX_LEN};
use crate::decimal::Decimal;
use crate::error::{EngineError, Result, SqlState};
use crate::interval::Interval;
use crate::timestamp::{
    NEG_INFINITY, POS_INFINITY, civil_from_days, civil_from_micros, days_from_civil,
};

const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;
const MICROS_PER_DAY: i64 = SECS_PER_DAY * MICROS_PER_SEC;
const MICROS_PER_MIN: i64 = 60 * MICROS_PER_SEC;
const MICROS_PER_HOUR: i64 = 3_600 * MICROS_PER_SEC;
// 365.25 days/year * 86400 s/day = 31_557_600 s (integral — PG's interval-epoch year).
const SECS_PER_INTERVAL_YEAR: i64 = 31_557_600;
const SECS_PER_INTERVAL_MONTH: i64 = 30 * SECS_PER_DAY; // PG's interval-epoch 30-day month.
/// The Julian Day number of 1970-01-01 (PG's `UNIX_EPOCH_JDATE`).
const UNIX_EPOCH_JD: i64 = 2_440_588;

fn feature(detail: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::FeatureNotSupported, detail.into())
}
fn invalid_param(detail: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::InvalidParameterValue, detail.into())
}

/// Astronomical year (0 = 1 BC) → PostgreSQL's year numbering (no year 0; 1 BC = -1). EXTRACT and
/// the year-group fields report this. `astro <= 0` is BC.
fn to_pg_year(astro: i64) -> i64 {
    if astro <= 0 { astro - 1 } else { astro }
}
/// PostgreSQL year numbering → astronomical year (inverse of [`to_pg_year`]; a negative PG year is
/// BC, shifted up by one to skip the missing year 0).
fn from_pg_year(pg: i64) -> i64 {
    if pg < 0 { pg + 1 } else { pg }
}

/// Build a `numeric` from an unscaled `i64` and a scale (value = `unscaled * 10^-scale`). Handles
/// the sign; small magnitudes pad with leading zeros (e.g. `(5, 6)` → `0.000005`).
fn decimal_scaled(unscaled: i64, scale: u32) -> Decimal {
    Decimal::from_digits_scale(unscaled < 0, &unscaled.unsigned_abs().to_string(), scale)
}

// --- weekday / ISO-week helpers (0=Sunday for dow; 1=Monday for isodow) -------

/// Day of week, 0 = Sunday .. 6 = Saturday (PG `dow`). 1970-01-01 was a Thursday (=4).
fn dow_sun0(days_since_epoch: i64) -> i64 {
    (days_since_epoch + 4).rem_euclid(7)
}
/// ISO day of week, 1 = Monday .. 7 = Sunday (PG `isodow`).
fn isodow_mon1(days_since_epoch: i64) -> i64 {
    (days_since_epoch + 3).rem_euclid(7) + 1
}

/// `p(y)` helper for the ISO-8601 week count (the weekday of 31 December, 0-based from a fixed
/// anchor). Used only by [`weeks_in_iso_year`].
fn iso_p(y: i64) -> i64 {
    (y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)).rem_euclid(7)
}
/// Number of ISO weeks (52 or 53) in the ISO-week-numbering year `y`.
fn weeks_in_iso_year(y: i64) -> i64 {
    if iso_p(y) == 4 || iso_p(y - 1) == 3 {
        53
    } else {
        52
    }
}

/// ISO week number + ISO week-numbering year for a civil date (astronomical year). Returns
/// `(iso_week, iso_year_astronomical)`.
fn iso_week_year(y: i64, mo: u32, d: u32) -> (i64, i64) {
    let days = days_from_civil(y, mo as i64, d as i64);
    let ordinal = days - days_from_civil(y, 1, 1) + 1; // day of year, 1-based
    let weekday = isodow_mon1(days); // 1=Mon..7=Sun
    let week = (ordinal - weekday + 10).div_euclid(7);
    if week < 1 {
        (weeks_in_iso_year(y - 1), y - 1)
    } else if week > weeks_in_iso_year(y) {
        (1, y + 1)
    } else {
        (week, y)
    }
}

// --- unit names --------------------------------------------------------------

/// What a unit name means to `EXTRACT` / `date_part` (spec/design/timezones.md §9.6).
#[derive(Clone, Copy, PartialEq, Debug)]
enum FieldName {
    /// a unit or field, by its canonical name (`hours` → `hour`, `jd` → `julian`, `epoch`)
    Unit(&'static str),
    /// a reserved word other than `epoch` (`infinity`, `now`, `today`, …)
    Reserved,
    /// anything else
    Unknown,
}

/// The unit name as PostgreSQL's error messages print it: ASCII-lowercased
/// (`downcase_truncate_identifier`; a non-ASCII letter keeps its case).
fn lower_unit(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// PostgreSQL's `datebsearch`: the entry whose token equals the first `TOKEN_MAX_LEN` bytes of the
/// lowercased `name` (so `microseconds` finds `microsecon`).
fn search_tokens(table: &'static [DtToken], name: &str) -> Option<&'static DtToken> {
    let key = &name.as_bytes()[..name.len().min(TOKEN_MAX_LEN)];
    table
        .binary_search_by(|t| t.token.as_bytes().cmp(key))
        .ok()
        .map(|i| &table[i])
}

/// PostgreSQL's `DecodeUnits` over a lowercased name: the canonical unit a `deltatktbl` units entry
/// names (`hrs` → `hour`), else `None`. `date_trunc` reads unit names this way.
fn decode_units(name: &str) -> Option<&'static str> {
    match search_tokens(DELTA_TOKENS, name) {
        Some(t) if t.kind == "units" => Some(t.value),
        _ => None,
    }
}

/// `EXTRACT` / `date_part`'s field lookup over a lowercased name: PostgreSQL's `DecodeUnits`, then
/// `DecodeSpecial` (`datetktbl`) when the name is not in `deltatktbl` at all.
fn decode_field(name: &str) -> FieldName {
    if let Some(t) = search_tokens(DELTA_TOKENS, name) {
        return if t.kind == "units" {
            FieldName::Unit(t.value)
        } else {
            FieldName::Unknown
        };
    }
    match search_tokens(DATE_TOKENS, name) {
        Some(t) if t.kind == "units" => FieldName::Unit(t.value),
        Some(t) if t.kind == "reserv" && t.value == "epoch" => FieldName::Unit("epoch"),
        Some(t) if t.kind == "reserv" => FieldName::Reserved,
        _ => FieldName::Unknown,
    }
}

/// The canonical field an `EXTRACT` / `date_part` field name resolves to (`jd` → `julian`), or
/// `None` when it names no field.
pub fn canonical_field(field: &str) -> Option<&'static str> {
    match decode_field(&lower_unit(field)) {
        FieldName::Unit(u) => Some(u),
        _ => None,
    }
}

fn not_recognized(name: &str, type_name: &str) -> EngineError {
    invalid_param(format!(
        "unit \"{name}\" not recognized for type {type_name}"
    ))
}

fn not_supported(name: &str, type_name: &str) -> EngineError {
    feature(format!(
        "unit \"{name}\" not supported for type {type_name}"
    ))
}

/// A timestamp family's type name, as PostgreSQL's unit messages print it.
fn timestamp_type_name(tz: bool) -> &'static str {
    if tz {
        "timestamp with time zone"
    } else {
        "timestamp without time zone"
    }
}

// --- date_trunc --------------------------------------------------------------

/// `date_trunc(unit, timestamp)` / the wall-clock half of `date_trunc(unit, timestamptz[, zone])`:
/// truncate the wall-clock instant `micros` down to the start of `unit` (`tz` names the source type
/// in messages). The unit is resolved first, as PostgreSQL does: an unrecognized unit is `22023`
/// (`invalid_parameter_value`) and a `timezone*` unit `0A000`, even over `±infinity`, which
/// otherwise passes through unchanged.
pub fn date_trunc_micros(unit: &str, micros: i64, tz: bool) -> Result<i64> {
    let name = lower_unit(unit);
    let Some(u) = decode_units(&name) else {
        return Err(not_recognized(&name, timestamp_type_name(tz)));
    };
    if micros == POS_INFINITY || micros == NEG_INFINITY {
        // Validate the unit against a finite probe, then pass the infinity through.
        date_trunc_micros(unit, 0, tz)?;
        return Ok(micros);
    }
    let (y, mo, d, h, mi, s, us) = civil_from_micros(micros);
    // Recompose a wall-clock instant from civil fields (floor-consistent with civil_from_micros).
    let rebuild = |y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32, us: u32| -> i64 {
        days_from_civil(y, mo as i64, d as i64) * MICROS_PER_DAY
            + h as i64 * MICROS_PER_HOUR
            + mi as i64 * MICROS_PER_MIN
            + s as i64 * MICROS_PER_SEC
            + us as i64
    };
    let out = match u {
        "microseconds" => micros,
        "milliseconds" => rebuild(y, mo, d, h, mi, s, (us / 1000) * 1000),
        "second" => rebuild(y, mo, d, h, mi, s, 0),
        "minute" => rebuild(y, mo, d, h, mi, 0, 0),
        "hour" => rebuild(y, mo, d, h, 0, 0, 0),
        "day" => rebuild(y, mo, d, 0, 0, 0, 0),
        "week" => {
            // Back to Monday, time zeroed. monday0 = days since the most recent Monday.
            let days = days_from_civil(y, mo as i64, d as i64);
            let monday0 = (days + 3).rem_euclid(7); // Mon=0..Sun=6
            (days - monday0) * MICROS_PER_DAY
        }
        "month" => rebuild(y, mo, 1, 0, 0, 0, 0),
        "quarter" => rebuild(y, ((mo - 1) / 3) * 3 + 1, 1, 0, 0, 0, 0),
        "year" => rebuild(y, 1, 1, 0, 0, 0, 0),
        "decade" => {
            let d10 = extract_decade(to_pg_year(y));
            let start_tm = if d10 >= 1 { d10 * 10 } else { d10 * 10 - 1 };
            rebuild(from_pg_year(start_tm), 1, 1, 0, 0, 0, 0)
        }
        "century" => {
            let c = extract_century(to_pg_year(y));
            let start_tm = if c >= 1 { (c - 1) * 100 + 1 } else { c * 100 };
            rebuild(from_pg_year(start_tm), 1, 1, 0, 0, 0, 0)
        }
        "millennium" => {
            let m = extract_millennium(to_pg_year(y));
            let start_tm = if m >= 1 { (m - 1) * 1000 + 1 } else { m * 1000 };
            rebuild(from_pg_year(start_tm), 1, 1, 0, 0, 0, 0)
        }
        _ => return Err(not_supported(&name, timestamp_type_name(tz))),
    };
    Ok(out)
}

/// `date_trunc(unit, interval)` — truncate an interval's fields down to `unit`. `week` and the
/// `timezone*` units are `0A000` (PG does not support them for intervals); the year-group units
/// truncate the months field to a multiple of 10/100/1000 years. An unrecognized unit is `22023`.
pub fn date_trunc_interval(unit: &str, iv: Interval) -> Result<Interval> {
    let name = lower_unit(unit);
    let Some(u) = decode_units(&name) else {
        return Err(not_recognized(&name, "interval"));
    };
    let months = iv.months as i64;
    let days = iv.days as i64;
    let micros = iv.micros;
    let keep_md = |micros: i64| Interval {
        months: iv.months,
        days: iv.days,
        micros,
    };
    let out = match u {
        "microseconds" => iv,
        "milliseconds" => keep_md((micros / 1000) * 1000),
        "second" => keep_md((micros / MICROS_PER_SEC) * MICROS_PER_SEC),
        "minute" => keep_md((micros / MICROS_PER_MIN) * MICROS_PER_MIN),
        "hour" => keep_md((micros / MICROS_PER_HOUR) * MICROS_PER_HOUR),
        "day" => keep_md(0),
        "week" => {
            return Err(not_supported(&name, "interval")
                .with_detail(Some("Months usually have fractional weeks.".to_string())));
        }
        "month" => Interval {
            months: iv.months,
            days: 0,
            micros: 0,
        },
        "quarter" => Interval {
            months: ((months / 3) * 3) as i32,
            days: 0,
            micros: 0,
        },
        "year" => Interval {
            months: ((months / 12) * 12) as i32,
            days: 0,
            micros: 0,
        },
        "decade" => Interval {
            months: ((months / 120) * 120) as i32,
            days: 0,
            micros: 0,
        },
        "century" => Interval {
            months: ((months / 1200) * 1200) as i32,
            days: 0,
            micros: 0,
        },
        "millennium" => Interval {
            months: ((months / 12000) * 12000) as i32,
            days: 0,
            micros: 0,
        },
        _ => return Err(not_supported(&name, "interval")),
    };
    let _ = days;
    Ok(out)
}

// --- EXTRACT -----------------------------------------------------------------

/// The source value of an `EXTRACT(field FROM source)` (spec/design/timezones.md §9.2). For a
/// `timestamptz` the caller supplies the wall-clock `local` micros (already converted into the
/// session zone), the raw `instant` (for `epoch`), and the zone `offset_secs` (for the `timezone*`
/// fields); for a `timestamp` only the wall-clock micros.
pub enum ExtractSrc {
    Timestamp(i64),
    Timestamptz {
        instant: i64,
        local: i64,
        offset_secs: i64,
    },
    Date(i32),
    Interval(Interval),
}

/// Split wall-clock micros into the Julian Day number of its date (PG's `date2j`) and the
/// microseconds into that day.
fn julian_parts(micros: i64) -> (i64, i64) {
    let days = micros.div_euclid(MICROS_PER_DAY);
    (days + UNIX_EPOCH_JD, micros - days * MICROS_PER_DAY)
}

/// `EXTRACT(julian FROM timestamp)` over finite wall-clock micros: the Julian Day number plus the
/// fraction of the day, exactly as PG's numeric path computes it — `jd + (time-of-day micros /
/// 86400000000)`, the division at numeric's `select_div_scale`.
fn extract_julian(micros: i64) -> Result<Decimal> {
    let (jd, tod_us) = julian_parts(micros);
    let frac = Decimal::from_i64(tod_us).div(&Decimal::from_i64(MICROS_PER_DAY))?;
    Decimal::from_i64(jd).add(&frac)
}

/// `date_part('julian', timestamp)` over finite wall-clock micros — PG's float8 formula verbatim
/// (`jd + (seconds + fsec/1e6) / 86400.0`, not the numeric value rounded), so the result is PG's to
/// the bit. The caller handles an infinite source through `extract_part`.
pub fn date_part_julian(micros: i64) -> f64 {
    let (jd, tod_us) = julian_parts(micros);
    let secs = tod_us / MICROS_PER_SEC;
    let fsec = tod_us % MICROS_PER_SEC;
    jd as f64 + (secs as f64 + fsec as f64 / 1000000.0) / SECS_PER_DAY as f64
}

/// The result of extracting a field: a finite value, or — over a `±infinity` source — PostgreSQL's
/// `NonFiniteTimestampTzPart` answer (timezones.md §9.2): NULL for an oscillating field (`month`,
/// `hour`, `dow`, …), `±Infinity` with the source's sign for a monotonically-increasing one
/// (`year`, `decade`, `century`, `millennium`, `julian`, `isoyear`, `epoch`).
pub enum ExtractOut {
    Value(Decimal),
    Null,
    Infinite { negative: bool },
}

/// `EXTRACT(field FROM source)` → `numeric` (NULL is `None`). The shared kernel's `±Infinity` traps
/// `22003`: jed's `decimal` is finite-only (decimal.md §2), where PostgreSQL returns numeric
/// `±Infinity` — a documented divergence (timezones.md §9.2). An oscillating field over an infinite
/// source is NULL, as in PG.
pub fn extract_field(field: &str, src: ExtractSrc) -> Result<Option<Decimal>> {
    let what = match src {
        ExtractSrc::Date(_) => "date",
        _ => "timestamp",
    };
    match extract_part(field, src)? {
        ExtractOut::Value(d) => Ok(Some(d)),
        ExtractOut::Null => Ok(None),
        ExtractOut::Infinite { .. } => Err(EngineError::new(
            SqlState::NumericValueOutOfRange,
            format!("cannot extract field from an infinite {what}"),
        )),
    }
}

/// The shared `EXTRACT` / `date_part` kernel. The field name is resolved first, through
/// PostgreSQL's unit tables (so `hours`, `msec`, `jd` are `hour`, `milliseconds`, `julian` —
/// timezones.md §9.6); the field-validity matrix then matches PostgreSQL: an unsupported field for
/// the source type is `0A000`, an unrecognized field is `22023`, and both are decided before an
/// infinite source is (PG decodes the unit first). For a `timestamptz` source every field is
/// computed in the (already-applied) session zone except `epoch` (the instant) and the `timezone*`
/// fields (the zone offset).
pub fn extract_part(field: &str, src: ExtractSrc) -> Result<ExtractOut> {
    let name = lower_unit(field);
    let type_name = match src {
        ExtractSrc::Timestamp(_) => timestamp_type_name(false),
        ExtractSrc::Timestamptz { .. } => timestamp_type_name(true),
        ExtractSrc::Date(_) => "date",
        ExtractSrc::Interval(_) => "interval",
    };
    let unit = match decode_field(&name) {
        FieldName::Unit(u) => u,
        // A reserved word other than `epoch` is unsupported for the date/time types but
        // unrecognized for an interval — PostgreSQL's interval_part_common checks only epoch.
        FieldName::Reserved if !matches!(src, ExtractSrc::Interval(_)) => {
            return Err(not_supported(&name, type_name));
        }
        _ => return Err(not_recognized(&name, type_name)),
    };
    match src {
        ExtractSrc::Timestamp(micros) => extract_datetime(unit, &name, micros, None),
        ExtractSrc::Timestamptz {
            instant,
            local,
            offset_secs,
        } => extract_datetime(unit, &name, local, Some((instant, offset_secs))),
        ExtractSrc::Date(days) => extract_date(unit, &name, days),
        ExtractSrc::Interval(iv) => extract_interval(unit, &name, iv).map(ExtractOut::Value),
    }
}

/// Whether `unit` increases monotonically with time — PostgreSQL's `NonFiniteTimestampTzPart`
/// split: these fields of a `±infinity` source are `±Infinity`; every other field it accepts
/// oscillates and is NULL.
fn is_monotonic_field(unit: &str) -> bool {
    matches!(
        unit,
        "year" | "decade" | "century" | "millennium" | "julian" | "isoyear" | "epoch"
    )
}

/// Shared timestamp/timestamptz field extraction of the canonical `unit` over the wall-clock
/// `micros` (`name` is the field as written, for messages). `tz` carries `(instant, offset_secs)`
/// for a `timestamptz` (so `epoch`/`timezone*` use the instant/offset); `None` for a `timestamp`
/// (those fields are then `0A000` / use the wall micros for `epoch`).
fn extract_datetime(
    unit: &str,
    name: &str,
    micros: i64,
    tz: Option<(i64, i64)>,
) -> Result<ExtractOut> {
    if micros == POS_INFINITY || micros == NEG_INFINITY {
        // PG's NonFiniteTimestampTzPart: every unit is accepted here, even `timezone` of a
        // timestamp (oscillating, so NULL).
        if is_monotonic_field(unit) {
            return Ok(ExtractOut::Infinite {
                negative: micros == NEG_INFINITY,
            });
        }
        return Ok(ExtractOut::Null);
    }
    let (y, mo, d, h, mi, s, us) = civil_from_micros(micros);
    let sec_us = s as i64 * MICROS_PER_SEC + us as i64;
    let days = days_from_civil(y, mo as i64, d as i64);
    let v = match unit {
        "microseconds" => decimal_scaled(sec_us, 0),
        "milliseconds" => decimal_scaled(sec_us, 3),
        "second" => decimal_scaled(sec_us, 6),
        "minute" => Decimal::from_i64(mi as i64),
        "hour" => Decimal::from_i64(h as i64),
        "day" => Decimal::from_i64(d as i64),
        "month" => Decimal::from_i64(mo as i64),
        "quarter" => Decimal::from_i64(((mo - 1) / 3 + 1) as i64),
        "year" => Decimal::from_i64(to_pg_year(y)),
        "decade" => Decimal::from_i64(extract_decade(to_pg_year(y))),
        "century" => Decimal::from_i64(extract_century(to_pg_year(y))),
        "millennium" => Decimal::from_i64(extract_millennium(to_pg_year(y))),
        "week" => Decimal::from_i64(iso_week_year(y, mo, d).0),
        "dow" => Decimal::from_i64(dow_sun0(days)),
        "isodow" => Decimal::from_i64(isodow_mon1(days)),
        "doy" => Decimal::from_i64(days - days_from_civil(y, 1, 1) + 1),
        "isoyear" => Decimal::from_i64(to_pg_year(iso_week_year(y, mo, d).1)),
        "epoch" => {
            // The instant (timestamptz) / the wall-clock micros (timestamp), in seconds, scale 6.
            let inst = tz.map(|(i, _)| i).unwrap_or(micros);
            decimal_scaled(inst, 6)
        }
        "timezone" | "timezone_hour" | "timezone_minute" => match tz {
            Some((_, off)) => match unit {
                "timezone" => Decimal::from_i64(off),
                "timezone_hour" => Decimal::from_i64(off / 3600),
                _ => Decimal::from_i64((off % 3600) / 60),
            },
            None => return Err(not_supported(name, timestamp_type_name(false))),
        },
        "julian" => extract_julian(micros)?,
        _ => return Err(not_supported(name, timestamp_type_name(tz.is_some()))),
    };
    Ok(ExtractOut::Value(v))
}

/// `EXTRACT(field FROM date)`: the calendar fields only — the time fields (`hour`/`minute`/… and
/// `timezone*`) are `0A000` (PG: "unit X not supported for type date"). `epoch` is `days * 86400`.
fn extract_date(unit: &str, name: &str, days: i32) -> Result<ExtractOut> {
    if days == crate::date::POS_INFINITY || days == crate::date::NEG_INFINITY {
        // PG's extract_date: the monotonic fields are ±Infinity, the date's oscillating fields
        // NULL, and every other field (the time and timezone fields) still 0A000.
        return match unit {
            "day" | "month" | "quarter" | "week" | "dow" | "isodow" | "doy" => Ok(ExtractOut::Null),
            _ if is_monotonic_field(unit) => Ok(ExtractOut::Infinite {
                negative: days == crate::date::NEG_INFINITY,
            }),
            _ => Err(not_supported(name, "date")),
        };
    }
    let days = days as i64;
    let (y, mo, d) = civil_from_days(days);
    let v = match unit {
        "day" => Decimal::from_i64(d as i64),
        "month" => Decimal::from_i64(mo as i64),
        "quarter" => Decimal::from_i64(((mo - 1) / 3 + 1) as i64),
        "year" => Decimal::from_i64(to_pg_year(y)),
        "decade" => Decimal::from_i64(extract_decade(to_pg_year(y))),
        "century" => Decimal::from_i64(extract_century(to_pg_year(y))),
        "millennium" => Decimal::from_i64(extract_millennium(to_pg_year(y))),
        "week" => Decimal::from_i64(iso_week_year(y, mo, d).0),
        "dow" => Decimal::from_i64(dow_sun0(days)),
        "isodow" => Decimal::from_i64(isodow_mon1(days)),
        "doy" => Decimal::from_i64(days - days_from_civil(y, 1, 1) + 1),
        "isoyear" => Decimal::from_i64(to_pg_year(iso_week_year(y, mo, d).1)),
        "epoch" => Decimal::from_i64(days * SECS_PER_DAY),
        "julian" => Decimal::from_i64(days + UNIX_EPOCH_JD),
        _ => return Err(not_supported(name, "date")),
    };
    Ok(ExtractOut::Value(v))
}

/// `EXTRACT(field FROM interval)` (spec/design/timezones.md §9.2). The interval's months/days/micros
/// components decompose independently (the time component is NOT carried into days). The date-of-the
/// fields (`dow`/`doy`/`isodow`/`isoyear`/`julian`/`timezone*`) are `0A000`. `epoch` uses PG's
/// 365.25-day year / 30-day month.
fn extract_interval(unit: &str, name: &str, iv: Interval) -> Result<Decimal> {
    let months = iv.months as i64;
    let days = iv.days as i64;
    let micros = iv.micros;
    let years = months / 12; // trunc toward zero
    let mo = months % 12; // trunc remainder (signed)
    let time_sec_us = micros % MICROS_PER_MIN; // micros within the minute (signed)
    let v = match unit {
        "microseconds" => decimal_scaled(time_sec_us, 0),
        "milliseconds" => decimal_scaled(time_sec_us, 3),
        "second" => decimal_scaled(time_sec_us, 6),
        "minute" => Decimal::from_i64((micros / MICROS_PER_MIN) % 60),
        "hour" => Decimal::from_i64(micros / MICROS_PER_HOUR),
        "day" => Decimal::from_i64(days),
        "week" => Decimal::from_i64(days / 7),
        "month" => Decimal::from_i64(mo),
        "quarter" => Decimal::from_i64(interval_quarter(months)),
        "year" => Decimal::from_i64(years),
        "decade" => Decimal::from_i64(years / 10),
        "century" => Decimal::from_i64(years / 100),
        "millennium" => Decimal::from_i64(years / 1000),
        "epoch" => {
            // (years*365.25d + mo*30d + days)*86400 s + time, exact (avoid i64 overflow by adding
            // the integer-seconds part as a Decimal).
            let int_secs =
                years * SECS_PER_INTERVAL_YEAR + mo * SECS_PER_INTERVAL_MONTH + days * SECS_PER_DAY;
            return Decimal::from_i64(int_secs).add(&decimal_scaled(micros, 6));
        }
        _ => return Err(not_supported(name, "interval")),
    };
    Ok(v)
}

// --- year-group field formulas (PG-exact, BC-aware on the PG year) ------------

/// `EXTRACT(decade)` of a PG-numbered year (no year 0). AD: `year/10`; BC: `-((8 - year)/10)`
/// (oracle-pinned — the asymmetric BC constant is PostgreSQL's).
fn extract_decade(pg_year: i64) -> i64 {
    if pg_year >= 0 {
        pg_year / 10
    } else {
        -((8 - pg_year) / 10)
    }
}
/// `EXTRACT(century)` of a PG-numbered year. AD: `(year+99)/100`; BC: `-((99 - year)/100)`.
fn extract_century(pg_year: i64) -> i64 {
    if pg_year >= 0 {
        (pg_year + 99) / 100
    } else {
        -((99 - pg_year) / 100)
    }
}
/// `EXTRACT(millennium)` of a PG-numbered year. AD: `(year+999)/1000`; BC: `-((999 - year)/1000)`.
fn extract_millennium(pg_year: i64) -> i64 {
    if pg_year >= 0 {
        (pg_year + 999) / 1000
    } else {
        -((999 - pg_year) / 1000)
    }
}

/// `EXTRACT(quarter FROM interval)` from the signed `months` field — PG's `interval_part_common`, which
/// works from the months count rather than the truncated month-of-year so that a negative interval's
/// quarter is the negation of the sign-reversed interval's: `(months % 12)/3 + 1` for `months >= 0`,
/// else `-((-months % 12)/3 + 1)` (so `-12 mons` is `-1`, not `1`; oracle-pinned).
fn interval_quarter(months: i64) -> i64 {
    if months >= 0 {
        (months % 12) / 3 + 1
    } else {
        -(((-months) % 12) / 3 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timestamp::{parse_timestamp, render_timestamp};

    fn ts(s: &str) -> i64 {
        parse_timestamp(s).unwrap()
    }
    fn ex(field: &str, micros: i64) -> String {
        extract_field(field, ExtractSrc::Timestamp(micros))
            .unwrap()
            .unwrap()
            .render()
    }
    fn ex_date(field: &str, days: i32) -> String {
        extract_field(field, ExtractSrc::Date(days))
            .unwrap()
            .unwrap()
            .render()
    }
    fn ex_iv(field: &str, months: i32, days: i32, micros: i64) -> String {
        extract_field(
            field,
            ExtractSrc::Interval(Interval {
                months,
                days,
                micros,
            }),
        )
        .unwrap()
        .unwrap()
        .render()
    }
    fn dt(unit: &str, micros: i64) -> String {
        render_timestamp(date_trunc_micros(unit, micros, false).unwrap())
    }

    #[test]
    fn extract_timestamp_fields() {
        let t = ts("2024-03-15 13:47:23.456789");
        assert_eq!(ex("microseconds", t), "23456789");
        assert_eq!(ex("milliseconds", t), "23456.789");
        assert_eq!(ex("second", t), "23.456789");
        assert_eq!(ex("minute", t), "47");
        assert_eq!(ex("hour", t), "13");
        assert_eq!(ex("day", t), "15");
        assert_eq!(ex("month", t), "3");
        assert_eq!(ex("quarter", t), "1");
        assert_eq!(ex("year", t), "2024");
        assert_eq!(ex("decade", t), "202");
        assert_eq!(ex("century", t), "21");
        assert_eq!(ex("millennium", t), "3");
        assert_eq!(ex("week", t), "11");
        assert_eq!(ex("dow", t), "5");
        assert_eq!(ex("isodow", t), "5");
        assert_eq!(ex("doy", t), "75");
        assert_eq!(ex("isoyear", t), "2024");
        assert_eq!(ex("epoch", t), "1710510443.456789");
        // whole-second scales
        let w = ts("2024-01-01 00:00:23");
        assert_eq!(ex("second", w), "23.000000");
        assert_eq!(ex("milliseconds", w), "23000.000");
        assert_eq!(ex("microseconds", w), "23000000");
        assert_eq!(ex("epoch", w), "1704067223.000000");
    }

    #[test]
    fn extract_iso_and_bc() {
        assert_eq!(
            ex_date("week", crate::date::parse_date("2024-12-30").unwrap()),
            "1"
        );
        assert_eq!(
            ex_date("isoyear", crate::date::parse_date("2024-12-30").unwrap()),
            "2025"
        );
        assert_eq!(
            ex_date("week", crate::date::parse_date("2023-01-01").unwrap()),
            "52"
        );
        assert_eq!(
            ex_date("isoyear", crate::date::parse_date("2023-01-01").unwrap()),
            "2022"
        );
        assert_eq!(
            ex_date("isodow", crate::date::parse_date("2023-01-01").unwrap()),
            "7"
        );
        // BC year-group (astronomical 0 = 1 BC, -43 = 44 BC, -99 = 100 BC)
        let bc44 = ts("0044-06-15 00:00:00 BC");
        assert_eq!(ex("year", bc44), "-44");
        assert_eq!(ex("decade", bc44), "-5");
        assert_eq!(ex("century", bc44), "-1");
        assert_eq!(ex("millennium", bc44), "-1");
        let bc1 = ts("0001-06-15 00:00:00 BC");
        assert_eq!(ex("decade", bc1), "0");
        assert_eq!(ex("century", bc1), "-1");
        // date epoch
        assert_eq!(
            ex_date("epoch", crate::date::parse_date("2024-01-01").unwrap()),
            "1704067200"
        );
    }

    #[test]
    fn extract_interval_fields() {
        // '3 years 5 mons 17 days 13:47:23.456789'
        let m = 3 * 12 + 5;
        let micros = 13 * MICROS_PER_HOUR + 47 * MICROS_PER_MIN + 23 * MICROS_PER_SEC + 456789;
        assert_eq!(ex_iv("day", m, 17, micros), "17");
        assert_eq!(ex_iv("hour", m, 17, micros), "13");
        assert_eq!(ex_iv("minute", m, 17, micros), "47");
        assert_eq!(ex_iv("second", m, 17, micros), "23.456789");
        assert_eq!(ex_iv("month", m, 17, micros), "5");
        assert_eq!(ex_iv("year", m, 17, micros), "3");
        assert_eq!(ex_iv("quarter", m, 17, micros), "2");
        assert_eq!(ex_iv("week", m, 17, micros), "2");
        assert_eq!(ex_iv("decade", 140 * 12, 0, 0), "14");
        assert_eq!(ex_iv("epoch", m, 17, micros), "109151243.456789");
        // negatives
        assert_eq!(ex_iv("quarter", -5, 0, 0), "-2");
        assert_eq!(ex_iv("epoch", 0, -1, -2 * MICROS_PER_HOUR), "-93600.000000");
        assert_eq!(ex_iv("second", 0, 0, -5_250_000), "-5.250000");
        // 25h not normalized into a day
        assert_eq!(ex_iv("hour", 0, 0, 25 * MICROS_PER_HOUR), "25");
    }

    #[test]
    fn date_trunc_values() {
        let t = ts("2024-08-15 13:47:23.456789");
        assert_eq!(dt("milliseconds", t), "2024-08-15 13:47:23.456");
        assert_eq!(dt("second", t), "2024-08-15 13:47:23");
        assert_eq!(dt("minute", t), "2024-08-15 13:47:00");
        assert_eq!(dt("hour", t), "2024-08-15 13:00:00");
        assert_eq!(dt("day", t), "2024-08-15 00:00:00");
        assert_eq!(dt("week", t), "2024-08-12 00:00:00");
        assert_eq!(dt("month", t), "2024-08-01 00:00:00");
        assert_eq!(dt("quarter", t), "2024-07-01 00:00:00");
        assert_eq!(dt("year", t), "2024-01-01 00:00:00");
        assert_eq!(dt("decade", t), "2020-01-01 00:00:00");
        assert_eq!(dt("century", t), "2001-01-01 00:00:00");
        assert_eq!(dt("millennium", t), "2001-01-01 00:00:00");
        // BC date_trunc
        assert_eq!(
            dt("decade", ts("0044-06-01 00:00:00 BC")),
            "0051-01-01 00:00:00 BC"
        );
        assert_eq!(
            dt("century", ts("0044-06-01 00:00:00 BC")),
            "0100-01-01 00:00:00 BC"
        );
        assert_eq!(
            dt("decade", ts("0001-06-01 00:00:00")),
            "0001-01-01 00:00:00 BC"
        );
    }

    #[test]
    fn date_trunc_interval_values() {
        let m = 3 * 12 + 5;
        let micros = 13 * MICROS_PER_HOUR + 47 * MICROS_PER_MIN + 23 * MICROS_PER_SEC + 456789;
        let r = |u: &str| {
            crate::interval::render_interval(
                &date_trunc_interval(
                    u,
                    Interval {
                        months: m,
                        days: 17,
                        micros,
                    },
                )
                .unwrap(),
            )
        };
        assert_eq!(r("day"), "3 years 5 mons 17 days");
        assert_eq!(r("month"), "3 years 5 mons");
        assert_eq!(r("quarter"), "3 years 3 mons");
        assert_eq!(r("year"), "3 years");
        assert_eq!(r("hour"), "3 years 5 mons 17 days 13:00:00");
        assert!(
            date_trunc_interval(
                "week",
                Interval {
                    months: m,
                    days: 17,
                    micros
                }
            )
            .is_err()
        );
        // decade/century/millennium -> zero
        assert_eq!(r("decade"), "00:00:00");
    }

    #[test]
    fn errors() {
        let t = ts("2024-03-15 13:47:23");
        assert_eq!(
            extract_field("hour", ExtractSrc::Date(0))
                .unwrap_err()
                .code(),
            "0A000"
        );
        assert_eq!(
            extract_field("bogus", ExtractSrc::Timestamp(t))
                .unwrap_err()
                .code(),
            "22023"
        );
        assert_eq!(
            date_trunc_micros("bogus", t, false).unwrap_err().code(),
            "22023"
        );
        assert_eq!(
            extract_field(
                "dow",
                ExtractSrc::Interval(Interval {
                    months: 0,
                    days: 1,
                    micros: 0
                })
            )
            .unwrap_err()
            .code(),
            "0A000"
        );
    }
}
