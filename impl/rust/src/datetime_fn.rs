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

// --- date_trunc --------------------------------------------------------------

/// `date_trunc(unit, timestamp)` / the wall-clock half of `date_trunc(unit, timestamptz[, zone])`:
/// truncate the wall-clock instant `micros` down to the start of `unit`. `±infinity` passes through
/// unchanged (PG). An unrecognized unit is `22023` (`invalid_parameter_value`).
pub fn date_trunc_micros(unit: &str, micros: i64) -> Result<i64> {
    if micros == POS_INFINITY || micros == NEG_INFINITY {
        return Ok(micros);
    }
    let u = unit.to_ascii_lowercase();
    let (y, mo, d, h, mi, s, us) = civil_from_micros(micros);
    // Recompose a wall-clock instant from civil fields (floor-consistent with civil_from_micros).
    let rebuild = |y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32, us: u32| -> i64 {
        days_from_civil(y, mo as i64, d as i64) * MICROS_PER_DAY
            + h as i64 * MICROS_PER_HOUR
            + mi as i64 * MICROS_PER_MIN
            + s as i64 * MICROS_PER_SEC
            + us as i64
    };
    let out = match u.as_str() {
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
        _ => return Err(invalid_param(format!("unit \"{unit}\" not recognized"))),
    };
    Ok(out)
}

/// `date_trunc(unit, interval)` — truncate an interval's fields down to `unit`. `week` is `0A000`
/// (PG does not support `week` truncation for intervals); the year-group units truncate the months
/// field to a multiple of 10/100/1000 years. An unrecognized unit is `22023`.
pub fn date_trunc_interval(unit: &str, iv: Interval) -> Result<Interval> {
    let u = unit.to_ascii_lowercase();
    let months = iv.months as i64;
    let days = iv.days as i64;
    let micros = iv.micros;
    let keep_md = |micros: i64| Interval {
        months: iv.months,
        days: iv.days,
        micros,
    };
    let out = match u.as_str() {
        "microseconds" => iv,
        "milliseconds" => keep_md((micros / 1000) * 1000),
        "second" => keep_md((micros / MICROS_PER_SEC) * MICROS_PER_SEC),
        "minute" => keep_md((micros / MICROS_PER_MIN) * MICROS_PER_MIN),
        "hour" => keep_md((micros / MICROS_PER_HOUR) * MICROS_PER_HOUR),
        "day" => keep_md(0),
        "week" => return Err(feature("unit \"week\" not supported for type interval")),
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
        _ => return Err(invalid_param(format!("unit \"{unit}\" not recognized"))),
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

/// `EXTRACT(field FROM source)` → `numeric`. The field-validity matrix matches PostgreSQL: an
/// unsupported field for the source type is `0A000`, an unrecognized field is `22023`. `julian` is a
/// deferred field on every type (`0A000`). For a `timestamptz` source every field is computed in the
/// (already-applied) session zone except `epoch` (the instant) and the `timezone*` fields (the zone
/// offset).
pub fn extract_field(field: &str, src: ExtractSrc) -> Result<Decimal> {
    let f = field.to_ascii_lowercase();
    match src {
        ExtractSrc::Timestamp(micros) => extract_datetime(&f, micros, None),
        ExtractSrc::Timestamptz {
            instant,
            local,
            offset_secs,
        } => extract_datetime_tz(&f, local, instant, offset_secs),
        ExtractSrc::Date(days) => extract_date(&f, days),
        ExtractSrc::Interval(iv) => extract_interval(&f, iv),
    }
}

/// Shared timestamp/timestamptz field extraction over the wall-clock `micros`. `tz` carries
/// `(instant, offset_secs)` for a `timestamptz` (so `epoch`/`timezone*` use the instant/offset);
/// `None` for a `timestamp` (those fields are then `0A000` / use the wall micros for `epoch`).
fn extract_datetime(field: &str, micros: i64, tz: Option<(i64, i64)>) -> Result<Decimal> {
    if micros == POS_INFINITY || micros == NEG_INFINITY {
        // jed's decimal is finite-only (decimal.md §2); PG returns ±Infinity here — a documented
        // divergence (timezones.md §9.2).
        return Err(EngineError::new(
            SqlState::NumericValueOutOfRange,
            "cannot extract field from an infinite timestamp",
        ));
    }
    let (y, mo, d, h, mi, s, us) = civil_from_micros(micros);
    let sec_us = s as i64 * MICROS_PER_SEC + us as i64;
    let days = days_from_civil(y, mo as i64, d as i64);
    let v = match field {
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
            Some((_, off)) => match field {
                "timezone" => Decimal::from_i64(off),
                "timezone_hour" => Decimal::from_i64(off / 3600),
                _ => Decimal::from_i64((off % 3600) / 60),
            },
            None => {
                return Err(feature(format!(
                    "unit \"{field}\" not supported for type timestamp without time zone"
                )));
            }
        },
        "julian" => {
            return Err(feature(
                "unit \"julian\" not supported yet (jed deferred)".to_string(),
            ));
        }
        _ => return Err(invalid_param(format!("unit \"{field}\" not recognized"))),
    };
    Ok(v)
}

fn extract_datetime_tz(field: &str, local: i64, instant: i64, offset_secs: i64) -> Result<Decimal> {
    extract_datetime(field, local, Some((instant, offset_secs)))
}

/// `EXTRACT(field FROM date)`: the calendar fields only — the time fields (`hour`/`minute`/… and
/// `timezone*`) are `0A000` (PG: "unit X not supported for type date"). `epoch` is `days * 86400`.
fn extract_date(field: &str, days: i32) -> Result<Decimal> {
    let days = days as i64;
    let (y, mo, d) = civil_from_days(days);
    let v = match field {
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
        "microseconds" | "milliseconds" | "second" | "minute" | "hour" | "timezone"
        | "timezone_hour" | "timezone_minute" | "julian" => {
            return Err(feature(format!(
                "unit \"{field}\" not supported for type date"
            )));
        }
        _ => return Err(invalid_param(format!("unit \"{field}\" not recognized"))),
    };
    Ok(v)
}

/// `EXTRACT(field FROM interval)` (spec/design/timezones.md §9.2). The interval's months/days/micros
/// components decompose independently (the time component is NOT carried into days). The date-of-the
/// fields (`dow`/`doy`/`isodow`/`isoyear`/`julian`/`timezone*`) are `0A000`. `epoch` uses PG's
/// 365.25-day year / 30-day month.
fn extract_interval(field: &str, iv: Interval) -> Result<Decimal> {
    let months = iv.months as i64;
    let days = iv.days as i64;
    let micros = iv.micros;
    let years = months / 12; // trunc toward zero
    let mo = months % 12; // trunc remainder (signed)
    let time_sec_us = micros % MICROS_PER_MIN; // micros within the minute (signed)
    let v = match field {
        "microseconds" => decimal_scaled(time_sec_us, 0),
        "milliseconds" => decimal_scaled(time_sec_us, 3),
        "second" => decimal_scaled(time_sec_us, 6),
        "minute" => Decimal::from_i64((micros / MICROS_PER_MIN) % 60),
        "hour" => Decimal::from_i64(micros / MICROS_PER_HOUR),
        "day" => Decimal::from_i64(days),
        "week" => Decimal::from_i64(days / 7),
        "month" => Decimal::from_i64(mo),
        "quarter" => Decimal::from_i64(interval_quarter(mo)),
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
        "dow" | "isodow" | "doy" | "isoyear" | "julian" | "timezone" | "timezone_hour"
        | "timezone_minute" => {
            return Err(feature(format!(
                "unit \"{field}\" not supported for type interval"
            )));
        }
        _ => return Err(invalid_param(format!("unit \"{field}\" not recognized"))),
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

/// `EXTRACT(quarter FROM interval)` from the month-of-year `mo` (signed, ±11). PG's discontinuous
/// formula: `1` at `mo == 0`, else `sign(mo) * (|mo|/3 + 1)` (oracle-pinned).
fn interval_quarter(mo: i64) -> i64 {
    if mo == 0 {
        1
    } else {
        mo.signum() * (mo.abs() / 3 + 1)
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
            .render()
    }
    fn ex_date(field: &str, days: i32) -> String {
        extract_field(field, ExtractSrc::Date(days))
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
        .render()
    }
    fn dt(unit: &str, micros: i64) -> String {
        render_timestamp(date_trunc_micros(unit, micros).unwrap())
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
        assert_eq!(date_trunc_micros("bogus", t).unwrap_err().code(), "22023");
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
