//! The `date` calendar type — parsing and rendering (spec/design/date.md). A date is an
//! `i32` count of days since the Unix epoch (1970-01-01), proleptic Gregorian. It is the
//! day-granular sibling of `timestamp` and **reuses timestamp's calendar core verbatim**
//! (`days_from_civil`/`civil_from_days`, same epoch — spec/design/timestamp.md §2), so the two
//! types cannot drift.
//!
//! Unlike timestamp, a date keeps **only the date portion**: a time/offset in the input is
//! parsed and validated, then **discarded** — and `24:00:00` does **not** roll into the day
//! (PG behavior). No instant is ever computed, so a date spans a wider range than the i64-µs
//! timestamp (finite `i32::MIN+1 ..= i32::MAX-1`).

use crate::error::Result;
use crate::timestamp::{
    civil_from_days, days_from_civil, days_in_month, expect, field_overflow, invalid_format,
    read_frac, read_uint, trim_ascii_ws,
};

/// The `-infinity` sentinel — the smallest `i32`, sorts before every finite date.
pub const NEG_INFINITY: i32 = i32::MIN;
/// The `+infinity` sentinel — the largest `i32`, sorts after every finite date.
pub const POS_INFINITY: i32 = i32::MAX;

/// Finite day counts occupy `i32::MIN+1 ..= i32::MAX-1`; the extremes are reserved for ±infinity.
const MIN_FINITE: i64 = (i32::MIN + 1) as i64;
const MAX_FINITE: i64 = (i32::MAX - 1) as i64;

/// Parse a `date` literal to its i32 day count since 1970-01-01. The grammar is the full
/// timestamp literal grammar (spec/design/timestamp.md §3), but only the date portion is kept:
/// a trailing time and/or offset is validated then discarded, and `24:00:00` does not advance
/// the day. Malformed syntax traps `22007`; an out-of-range field or a day count beyond the
/// finite i32 range traps `22008`.
pub fn parse_date(input: &str) -> Result<i32> {
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

    let bad = || invalid_format("invalid input syntax for type date");

    // optional time — validated for syntax/range, then DISCARDED (the day is taken from the
    // date fields directly; 24:00:00 does not advance it).
    let mut hour: i64 = 0;
    let mut minute: i64 = 0;
    let mut second: i64 = 0;
    let mut micro: i64 = 0;
    if i < b.len() && (b[i] == b' ' || b[i] == b'T' || b[i] == b't') {
        i += 1;
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

    // optional offset (Z / +HH[:MM[:SS]] / -HH[:MM[:SS]]) — validated, then DISCARDED (never
    // applied, so it cannot shift the day; PG behavior).
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' => {
                i += 1;
            }
            b'+' | b'-' => {
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
            }
            _ => return Err(bad()),
        }
    }
    if i != b.len() {
        return Err(bad());
    }

    // Field validation (range errors are 22008). The year magnitude cap (a date spans ≈ ±5.88M
    // years, far wider than timestamp's ±294k) is only an i64-overflow guard for `days_from_civil`;
    // the real bound is the i32 day-range check below, which rejects any out-of-range date.
    if !(1..=9_999_999).contains(&year) {
        return Err(field_overflow("year out of range"));
    }
    if !(1..=12).contains(&month) {
        return Err(field_overflow("month out of range"));
    }
    let astro = if bc { 1 - year } else { year };
    if day < 1 || day > days_in_month(astro, month as u32) as i64 {
        return Err(field_overflow("day out of range for month"));
    }
    // hour 0..=23, plus exactly 24:00:00 (a valid end-of-day; unlike timestamp it does NOT
    // advance the date — the day comes from the date fields directly).
    let allow24 = hour == 24 && minute == 0 && second == 0 && micro == 0;
    if hour > 23 && !allow24 {
        return Err(field_overflow("hour out of range"));
    }
    if minute > 59 {
        return Err(field_overflow("minute out of range"));
    }
    if second > 59 {
        return Err(field_overflow("second out of range")); // no leap seconds (:60)
    }

    let days = days_from_civil(astro, month, day);
    if !(MIN_FINITE..=MAX_FINITE).contains(&days) {
        return Err(field_overflow("date out of range"));
    }
    Ok(days as i32)
}

/// Render a `date` value (i32 days since 1970-01-01) to its canonical `YYYY-MM-DD` text
/// (BC suffix for an astronomical year ≤ 0; ±infinity render as the bare words).
pub fn render_date(days: i32) -> String {
    if days == NEG_INFINITY {
        return "-infinity".to_string();
    }
    if days == POS_INFINITY {
        return "infinity".to_string();
    }
    let (y, mo, d) = civil_from_days(days as i64);
    let (displayed, era) = if y <= 0 { (1 - y, " BC") } else { (y, "") };
    format!("{displayed:04}-{mo:02}-{d:02}{era}")
}
