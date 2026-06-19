//! The `timestamp` / `timestamptz` calendar math, parsing, and rendering
//! (spec/design/timestamp.md). Both types are an `i64` count of microseconds since the
//! Unix epoch (1970-01-01 00:00:00 UTC), proleptic Gregorian, no leap seconds.
//!
//! This is a §8 determinism hotspot: the civil↔instant conversion (Howard Hinnant's
//! `days_from_civil`/`civil_from_days`), the parse grammar, and the render format must be
//! byte-identical across the Rust/Go/TS cores. The civil↔days path uses **truncating**
//! division (Rust `/` toward zero) paired with the Hinnant -399/-146096 adjustment; the
//! instant↔civil decomposition uses **floor** division (`div_euclid`/`rem_euclid`).

use crate::error::{EngineError, Result, SqlState};

/// The `-infinity` sentinel — the smallest `i64`, sorts before every finite instant.
pub const NEG_INFINITY: i64 = i64::MIN;
/// The `+infinity` sentinel — the largest `i64`, sorts after every finite instant.
pub const POS_INFINITY: i64 = i64::MAX;

const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

// --- calendar core -----------------------------------------------------------

/// Proleptic-Gregorian leap-year test on an astronomical year (…, -1, 0, 1, …).
fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Days in `month` (1..=12) of astronomical year `y`.
pub(crate) fn days_in_month(y: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Days since 1970-01-01 for the civil date `(y, m, d)` (Hinnant). `y` is the astronomical
/// year; `/` is truncating, paired with the `y-399` adjustment (= floor for negative `y`).
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + (d - 1); // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Civil date `(year, month, day)` from days since 1970-01-01 (inverse of `days_from_civil`).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

/// Decompose an instant (µs since epoch) into civil fields, using **floor** division so
/// pre-1970 / BC instants decompose correctly (`us` is always 0..=999_999).
pub(crate) fn civil_from_micros(t: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let us = t.rem_euclid(MICROS_PER_SEC);
    let secs = t.div_euclid(MICROS_PER_SEC);
    let sod = secs.rem_euclid(SECS_PER_DAY);
    let days = secs.div_euclid(SECS_PER_DAY);
    let (y, mo, d) = civil_from_days(days);
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let s = sod % 60;
    (y, mo, d, h as u32, mi as u32, s as u32, us as u32)
}

// --- parsing -----------------------------------------------------------------

pub(crate) fn invalid_format(detail: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::InvalidDatetimeFormat, detail.into())
}

pub(crate) fn field_overflow(detail: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::DatetimeFieldOverflow, detail.into())
}

/// An ASCII-whitespace byte (space, tab, LF, FF, CR) — the fixed set trimmed from the ends
/// of a datetime literal, identical across cores (not locale/Unicode-dependent).
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r')
}

pub(crate) fn trim_ascii_ws(s: &str) -> &str {
    let b = s.as_bytes();
    let mut start = 0;
    let mut end = b.len();
    while start < end && is_ws(b[start]) {
        start += 1;
    }
    while end > start && is_ws(b[end - 1]) {
        end -= 1;
    }
    &s[start..end]
}

/// Read one run of ASCII digits at `*i` as an `i64` (checked). Empty run → 22007; a value
/// that overflows `i64` → 22008.
pub(crate) fn read_uint(b: &[u8], i: &mut usize) -> Result<i64> {
    let start = *i;
    let mut v: i64 = 0;
    while *i < b.len() && b[*i].is_ascii_digit() {
        v = v
            .checked_mul(10)
            .and_then(|v| v.checked_add((b[*i] - b'0') as i64))
            .ok_or_else(|| field_overflow("numeric field too large"))?;
        *i += 1;
    }
    if *i == start {
        return Err(invalid_format("expected a number"));
    }
    Ok(v)
}

pub(crate) fn expect(b: &[u8], i: &mut usize, c: u8) -> Result<()> {
    if *i < b.len() && b[*i] == c {
        *i += 1;
        Ok(())
    } else {
        Err(invalid_format(format!("expected '{}'", c as char)))
    }
}

/// Parse the fractional-seconds digits after the `.` into microseconds (0..=1_000_000;
/// 1_000_000 means the rounding carried into the next second). 0–6 digits are exact;
/// 7+ digits round to µs **half away from zero** (the 7th digit `>= 5` rounds up).
pub(crate) fn read_frac(b: &[u8], i: &mut usize) -> Result<i64> {
    let start = *i;
    while *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
    }
    let digits = &b[start..*i];
    if digits.is_empty() {
        return Err(invalid_format("expected fractional digits after '.'"));
    }
    let mut us: i64 = 0;
    for k in 0..6 {
        us *= 10;
        if k < digits.len() {
            us += (digits[k] - b'0') as i64;
        }
    }
    if digits.len() > 6 && digits[6] >= b'5' {
        us += 1; // round half away from zero (us may reach 1_000_000 and carry)
    }
    Ok(us)
}

/// Parse a timestamp/timestamptz literal to µs since the epoch. For `timestamptz`
/// (`apply_offset = true`) a trailing offset normalizes the wall clock to UTC; for
/// `timestamp` an offset is parsed/validated but **ignored** (PG behavior). The type name
/// is used only for error messages.
fn parse(input: &str, apply_offset: bool, type_name: &str) -> Result<i64> {
    let s = trim_ascii_ws(input);
    let low = s.to_ascii_lowercase();

    // Special values (checked first).
    if low == "infinity" || low == "+infinity" {
        return Ok(POS_INFINITY);
    }
    if low == "-infinity" {
        return Ok(NEG_INFINITY);
    }

    // Trailing era ` BC` / ` AD` (case-insensitive); BC maps displayed year to astronomical.
    let mut bc = false;
    let mut body = s;
    if low.ends_with(" bc") {
        bc = true;
        body = trim_ascii_ws(&s[..s.len() - 3]);
    } else if low.ends_with(" ad") {
        body = trim_ascii_ws(&s[..s.len() - 3]);
    }

    let b = body.as_bytes();
    let mut i = 0usize;

    // date: year '-' month '-' day  (year is a magnitude; era sets the sign)
    let year = read_uint(b, &mut i)?;
    expect(b, &mut i, b'-')?;
    let month = read_uint(b, &mut i)?;
    expect(b, &mut i, b'-')?;
    let day = read_uint(b, &mut i)?;

    let bad = || invalid_format(format!("invalid input syntax for type {type_name}"));

    // optional time
    let mut hour: i64 = 0;
    let mut minute: i64 = 0;
    let mut second: i64 = 0;
    let mut micro: i64 = 0;
    let mut had_time = false;
    if i < b.len() && (b[i] == b' ' || b[i] == b'T' || b[i] == b't') {
        i += 1;
        had_time = true;
        hour = read_uint(b, &mut i)?;
        expect(b, &mut i, b':')?;
        minute = read_uint(b, &mut i)?;
        if i < b.len() && b[i] == b':' {
            i += 1;
            second = read_uint(b, &mut i)?;
            if i < b.len() && b[i] == b'.' {
                i += 1;
                micro = read_frac(b, &mut i)?;
            }
        }
    }

    // optional offset (Z / +HH[:MM[:SS]] / -HH[:MM[:SS]])
    let mut offset_secs: i64 = 0;
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' => {
                i += 1;
            }
            b'+' | b'-' => {
                let sign = if b[i] == b'-' { -1 } else { 1 };
                i += 1;
                let oh = read_uint(b, &mut i)?;
                let mut om = 0i64;
                let mut os = 0i64;
                if i < b.len() && b[i] == b':' {
                    i += 1;
                    om = read_uint(b, &mut i)?;
                    if i < b.len() && b[i] == b':' {
                        i += 1;
                        os = read_uint(b, &mut i)?;
                    }
                }
                if oh > 15 || om > 59 || os > 59 {
                    return Err(field_overflow("time zone offset out of range"));
                }
                offset_secs = sign * (oh * 3600 + om * 60 + os);
            }
            _ => return Err(bad()),
        }
    }
    if i != b.len() {
        return Err(bad());
    }
    let _ = had_time;

    // Field validation (range errors are 22008).
    if !(1..=999_999).contains(&year) {
        return Err(field_overflow("year out of range"));
    }
    if !(1..=12).contains(&month) {
        return Err(field_overflow("month out of range"));
    }
    let astro = if bc { 1 - year } else { year };
    if day < 1 || day > days_in_month(astro, month as u32) as i64 {
        return Err(field_overflow("day out of range for month"));
    }
    // hour 0..=23, plus exactly 24:00:00 (normalizes to next-day midnight)
    let extra_day = hour == 24 && minute == 0 && second == 0 && micro == 0;
    if hour > 23 && !extra_day {
        return Err(field_overflow("hour out of range"));
    }
    if minute > 59 {
        return Err(field_overflow("minute out of range"));
    }
    if second > 59 {
        return Err(field_overflow("second out of range")); // no leap seconds (:60)
    }
    let hour_part = if extra_day { 0 } else { hour };

    // Compose the instant in checked i64 arithmetic; any overflow is a range error.
    let mut days = days_from_civil(astro, month, day);
    if extra_day {
        days = days
            .checked_add(1)
            .ok_or_else(|| field_overflow("value out of range"))?;
    }
    let secs = days
        .checked_mul(SECS_PER_DAY)
        .and_then(|s| s.checked_add(hour_part * 3600 + minute * 60 + second))
        .ok_or_else(|| field_overflow("value out of range"))?;
    let mut micros = secs
        .checked_mul(MICROS_PER_SEC)
        .and_then(|m| m.checked_add(micro))
        .ok_or_else(|| field_overflow("value out of range"))?;
    if apply_offset {
        micros = offset_secs
            .checked_mul(MICROS_PER_SEC)
            .and_then(|o| micros.checked_sub(o))
            .ok_or_else(|| field_overflow("value out of range"))?;
    }
    if micros == NEG_INFINITY || micros == POS_INFINITY {
        return Err(field_overflow("value out of range")); // reserved for ±infinity
    }
    Ok(micros)
}

/// Parse a `timestamp` (zoneless) literal: an offset in the text is accepted and ignored.
pub fn parse_timestamp(s: &str) -> Result<i64> {
    parse(s, false, "timestamp")
}

/// Parse a `timestamptz` literal: a trailing offset normalizes the value to UTC.
pub fn parse_timestamptz(s: &str) -> Result<i64> {
    parse(s, true, "timestamptz")
}

// --- rendering ---------------------------------------------------------------

fn render(micros: i64, is_tz: bool) -> String {
    if micros == NEG_INFINITY {
        return "-infinity".to_string();
    }
    if micros == POS_INFINITY {
        return "infinity".to_string();
    }
    let (y, mo, d, h, mi, s, us) = civil_from_micros(micros);
    let (displayed, era) = if y <= 0 { (1 - y, " BC") } else { (y, "") };
    let mut out = format!("{displayed:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
    if us != 0 {
        let frac = format!("{us:06}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    if is_tz {
        out.push_str("+00");
    }
    out.push_str(era);
    out
}

/// Render a `timestamp` value to its canonical text.
pub fn render_timestamp(micros: i64) -> String {
    render(micros, false)
}

/// Render a `timestamptz` value to its canonical text (always UTC, fixed `+00`).
pub fn render_timestamptz(micros: i64) -> String {
    render(micros, true)
}
