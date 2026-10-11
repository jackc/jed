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

/// The `22008` raised when a zone shift (`timezone::instant_to_local_micros` /
/// `local_to_instant_micros`) leaves the finite timestamp range (timezones.md §6).
pub(crate) fn zone_shift_out_of_range() -> EngineError {
    field_overflow("timestamp out of range")
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

/// A parsed `timestamptz` input string: either already an instant (±infinity, or a finite wall
/// clock carrying an explicit offset, normalized to UTC) or a finite wall clock with **no** offset
/// (not yet checked against the sentinels), which the caller interprets in a zone.
pub enum TimestamptzInput {
    Instant(i64),
    Local(i64),
}

/// Parse a timestamp/timestamptz literal to µs since the epoch. For `timestamptz`
/// (`apply_offset = true`) a trailing offset normalizes the wall clock to UTC; for
/// `timestamp` an offset is parsed/validated but **ignored** (PG behavior). The type name
/// is used only for error messages.
fn parse(input: &str, apply_offset: bool, type_name: &str) -> Result<i64> {
    let (micros, offset_secs) = match parse_wall(input, type_name)? {
        Wall::Infinity(sentinel) => return Ok(sentinel),
        Wall::Finite(micros, offset_secs) => (micros, offset_secs),
    };
    match offset_secs {
        Some(off) if apply_offset => apply_utc_offset(micros, off),
        _ => finite_instant(micros),
    }
}

/// A parsed datetime string: a ±infinity sentinel, or a finite wall clock (µs since the epoch,
/// not yet checked against the sentinels) with its explicit trailing offset in seconds, `None`
/// when the text carries none.
enum Wall {
    Infinity(i64),
    Finite(i64, Option<i64>),
}

/// A finite instant may not land on a ±infinity sentinel (22008).
pub(crate) fn finite_instant(micros: i64) -> Result<i64> {
    if micros == NEG_INFINITY || micros == POS_INFINITY {
        return Err(field_overflow("value out of range")); // reserved for ±infinity
    }
    Ok(micros)
}

/// Normalize a finite wall clock carrying `offset_secs` to its UTC instant. Any overflow, or a
/// result landing on a ±infinity sentinel, is a 22008 range error.
fn apply_utc_offset(wall: i64, offset_secs: i64) -> Result<i64> {
    let micros = offset_secs
        .checked_mul(MICROS_PER_SEC)
        .and_then(|o| wall.checked_sub(o))
        .ok_or_else(|| field_overflow("value out of range"))?;
    finite_instant(micros)
}

/// Parse a datetime string to its [`Wall`] reading. Shared by every timestamp/timestamptz input
/// path; the type name is used only for error messages.
fn parse_wall(input: &str, type_name: &str) -> Result<Wall> {
    let s = trim_ascii_ws(input);
    let low = s.to_ascii_lowercase();

    // Special values (checked first).
    if low == "infinity" || low == "+infinity" {
        return Ok(Wall::Infinity(POS_INFINITY));
    }
    if low == "-infinity" {
        return Ok(Wall::Infinity(NEG_INFINITY));
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
    let mut offset_secs: Option<i64> = None;
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' => {
                i += 1;
                offset_secs = Some(0);
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
                offset_secs = Some(sign * (oh * 3600 + om * 60 + os));
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
    let micros = secs
        .checked_mul(MICROS_PER_SEC)
        .and_then(|m| m.checked_add(micro))
        .ok_or_else(|| field_overflow("value out of range"))?;
    Ok(Wall::Finite(micros, offset_secs))
}

/// Parse a `timestamp` (zoneless) literal: an offset in the text is accepted and ignored.
pub fn parse_timestamp(s: &str) -> Result<i64> {
    parse(s, false, "timestamp")
}

/// Parse a `timestamptz` literal: a trailing offset normalizes the value to UTC.
pub fn parse_timestamptz(s: &str) -> Result<i64> {
    parse(s, true, "timestamptz")
}

/// Parse a `timestamptz` input for the runtime `text → timestamptz` cast (types.md §5): an explicit
/// offset (or ±infinity) yields the instant, while an offset-less wall clock is returned as
/// [`TimestamptzInput::Local`] for the caller to interpret in the session zone, as PostgreSQL's
/// `timestamptz_in` does. (The literal path, [`parse_timestamptz`], still reads it as UTC.)
pub fn parse_timestamptz_input(s: &str) -> Result<TimestamptzInput> {
    Ok(match parse_wall(s, "timestamptz")? {
        Wall::Infinity(sentinel) => TimestamptzInput::Instant(sentinel),
        Wall::Finite(micros, Some(off)) => {
            TimestamptzInput::Instant(apply_utc_offset(micros, off)?)
        }
        Wall::Finite(micros, None) => TimestamptzInput::Local(micros),
    })
}

/// Build a zoneless `timestamp` (µs since the 1970 epoch) from calendar fields — the workhorse for
/// `make_timestamp` / `make_timestamptz` (functions.md §11; PostgreSQL `make_timestamp_internal`).
/// A negative `year` denotes BC (PG). Field validation mirrors the timestamp parser and traps 22008:
/// the year magnitude in `1..=999_999` (no year zero), `month` 1..=12, `day` valid for the month,
/// and the assembled time of day not past 24:00:00 — PG allows `hour = 24` or `sec = 60` so long as
/// the whole time of day stays within a day, enforced here by one total-of-day check. `sec` (double
/// precision) folds to micros by one correctly-rounded multiply + half-away round (the engine's one
/// mode — float.md §6); it differs from PG's `rint` only at an exact half-microsecond tie, which
/// realistic input never hits, so the result stays oracle-positive.
pub fn make_timestamp(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    sec: f64,
) -> Result<i64> {
    // Date fields (22008). A negative year is BC; year 0 has no AD/BC representation.
    if year == 0 {
        return Err(field_overflow("date field value out of range"));
    }
    let bc = year < 0;
    let mag = year.unsigned_abs() as i64; // |year|; the displayed magnitude
    if mag > 999_999 {
        return Err(field_overflow("date field value out of range"));
    }
    if !(1..=12).contains(&month) {
        return Err(field_overflow("date field value out of range"));
    }
    let astro = if bc { 1 - mag } else { mag };
    if day < 1 || day > days_in_month(astro, month as u32) as i64 {
        return Err(field_overflow("date field value out of range"));
    }
    // Time fields (22008), matching PG `float_time_overflows`: hour 0..=24, minute 0..=59, `sec`
    // rounded to micros in [0, 60_000_000] (so sec = 60 is allowed), and the whole time of day
    // (hours:mins:secs) ≤ 24:00:00.
    if !(0..=24).contains(&hour) || !(0..=59).contains(&minute) {
        return Err(field_overflow("time field value out of range"));
    }
    let sec_micros = (sec * MICROS_PER_SEC as f64).round(); // round-half-away (f64::round)
    if !sec_micros.is_finite() || !(0.0..=60_000_000.0).contains(&sec_micros) {
        return Err(field_overflow("time field value out of range"));
    }
    let tod_micros = ((hour * 60 + minute) * 60) * MICROS_PER_SEC + sec_micros as i64; // ≤ 9e10, no i64 overflow
    if tod_micros > SECS_PER_DAY * MICROS_PER_SEC {
        return Err(field_overflow("time field value out of range"));
    }
    // Compose the instant in checked i64 arithmetic; any overflow is a range error.
    let micros = days_from_civil(astro, month, day)
        .checked_mul(SECS_PER_DAY * MICROS_PER_SEC)
        .and_then(|d| d.checked_add(tod_micros))
        .ok_or_else(|| field_overflow("timestamp out of range"))?;
    if micros == NEG_INFINITY || micros == POS_INFINITY {
        return Err(field_overflow("timestamp out of range")); // reserved for ±infinity
    }
    Ok(micros)
}

// --- rendering ---------------------------------------------------------------

/// Render `micros` (a wall-clock reading) as ISO text. `offset` is the UT offset in seconds to
/// suffix for a `timestamptz` (`None` for a `timestamp`): PostgreSQL's `EncodeTimezone` spelling —
/// `+HH`, `+HH:MM` when the minutes are nonzero, `+HH:MM:SS` when the seconds are (an LMT offset),
/// placed before any ` BC` era.
fn render(micros: i64, offset: Option<i32>) -> String {
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
    if let Some(utoff) = offset {
        let sign = if utoff < 0 { '-' } else { '+' };
        let a = utoff.unsigned_abs();
        let (h, mi, s) = (a / 3600, a / 60 % 60, a % 60);
        out.push_str(&format!("{sign}{h:02}"));
        if s != 0 {
            out.push_str(&format!(":{mi:02}:{s:02}"));
        } else if mi != 0 {
            out.push_str(&format!(":{mi:02}"));
        }
    }
    out.push_str(era);
    out
}

/// Render a `timestamp` value to its canonical text.
pub fn render_timestamp(micros: i64) -> String {
    render(micros, None)
}

/// Render a `timestamptz` value to its canonical text (always UTC, fixed `+00`).
pub fn render_timestamptz(micros: i64) -> String {
    render(micros, Some(0))
}

/// Render a `timestamptz` instant as text in `zone` — the `timestamptz → text` cast, which follows
/// the session zone (timezones.md §9.5): the local wall clock plus the offset in effect at the
/// instant. ±infinity render bare; a finite instant whose local reading leaves the timestamp
/// range traps `22008` (timezones.md §6).
pub fn render_timestamptz_in(micros: i64, zone: &crate::timezone::ZoneRef) -> Result<String> {
    if micros == NEG_INFINITY || micros == POS_INFINITY {
        return Ok(render(micros, None));
    }
    let off = crate::timezone::offset_at_ref(zone, micros.div_euclid(MICROS_PER_SEC));
    let local = crate::timezone::instant_to_local_micros(zone, micros)
        .ok_or_else(zone_shift_out_of_range)?;
    Ok(render(local, Some(off.utoff)))
}
