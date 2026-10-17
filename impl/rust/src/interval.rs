//! The `interval` type — value model, parsing, and rendering (spec/design/interval.md).
//!
//! A value is PostgreSQL's three independent fields: `months` (i32), `days` (i32), and
//! `micros` (i64). They are kept separate so `+ 1 month` is calendar-aware (and distinct from
//! `+ 30 days`); comparison/ordering/dedup collapse them via the canonical 128-bit **span**
//! (1 month = 30 days, 1 day = 24 h). Parsing is a port of PostgreSQL's `interval_in` — its unit
//! syntax, the SQL-standard forms (`'1-2'`, `'1 1:00'`), and ISO 8601 (`'P1Y2M3DT4H5M6S'`).
//!
//! This is a §8 determinism hotspot: the fractional-unit cascade, the µs rounding (half away
//! from zero — the engine's one mode), and the render format must be byte-identical across the
//! Rust/Go/TS cores. All cascade arithmetic is exact integer math (no float in the value path).

use crate::datetime_units_gen::{INTERVAL_QUALIFIERS, IntervalQualifier};
use crate::error::{EngineError, Result, SqlState};
use crate::timestamp;

/// Microseconds in one second.
const MICROS_PER_SEC: i64 = 1_000_000;
/// Microseconds in one day (the canonical "1 day = 24 h" span weight).
pub const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SEC;
/// Days in one month for the canonical span / fractional cascade (PG `DAYS_PER_MONTH`).
pub const DAYS_PER_MONTH: i64 = 30;
/// Months in one year.
const MONTHS_PER_YEAR: i64 = 12;

/// An `interval` value — three independent fields (spec/design/interval.md §1). Comparison,
/// ordering, hashing, and dedup go through [`Interval::span`] (the canonical 128-bit
/// microsecond span), NOT the field triple, so `'1 mon' == '30 days' == '720:00:00'`. The
/// `PartialEq`/`Eq`/`Hash` impls below are span-canonical for exactly that reason — like
/// `Decimal`'s value-canonical traits — so a `Value::Interval` dedups by span while `render`
/// still prints each value's own fields.
#[derive(Clone, Copy, Debug)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    /// The canonical comparison key: a signed 128-bit microsecond span combining the three
    /// fields via 1 month = 30 days and 1 day = 24 h (PG `interval_cmp_value`). Total order,
    /// no NaN. Two intervals are equal iff their spans are equal.
    pub fn span(&self) -> i128 {
        let days = self.months as i128 * DAYS_PER_MONTH as i128 + self.days as i128;
        days * MICROS_PER_DAY as i128 + self.micros as i128
    }

    /// `self + other` — field-wise addition (PG keeps the fields independent, no justification).
    /// An `i32` month/day overflow traps `22008`.
    pub fn add(&self, other: &Interval) -> Result<Interval> {
        Ok(Interval {
            months: self
                .months
                .checked_add(other.months)
                .ok_or_else(|| field_overflow("interval out of range"))?,
            days: self
                .days
                .checked_add(other.days)
                .ok_or_else(|| field_overflow("interval out of range"))?,
            micros: self
                .micros
                .checked_add(other.micros)
                .ok_or_else(|| field_overflow("interval out of range"))?,
        })
    }

    /// `self - other` — field-wise subtraction. An `i32` month/day overflow traps `22008`.
    pub fn sub(&self, other: &Interval) -> Result<Interval> {
        Ok(Interval {
            months: self
                .months
                .checked_sub(other.months)
                .ok_or_else(|| field_overflow("interval out of range"))?,
            days: self
                .days
                .checked_sub(other.days)
                .ok_or_else(|| field_overflow("interval out of range"))?,
            micros: self
                .micros
                .checked_sub(other.micros)
                .ok_or_else(|| field_overflow("interval out of range"))?,
        })
    }

    /// The order-preserving KEY body for an interval (method `interval-span-i128`,
    /// spec/design/encoding.md §2.10). The 16-byte order-preserving encoding of the canonical
    /// 128-bit **span** ([`Interval::span`], §2) — `int-be-signflip` at i128 width: add the bias
    /// `2^127` and emit the sum as a 16-byte big-endian unsigned integer, mapping the signed span
    /// range monotonically onto `[0, 2^128)` so negatives sort below positives. Fixed-width 16, so
    /// self-delimiting with no escape/terminator (like uuid §2.7). Because the key is the **span**,
    /// two field-distinct but span-equal intervals (`'1 mon'` / `'30 days'`) produce identical bytes
    /// — a UNIQUE interval index treats them as one (the "equal but not identical" wrinkle, the
    /// decimal `1.5`/`1.50` precedent). A PK is NOT NULL, so the stored key is this bare 16-byte
    /// body; the §2.2 nullable slot prepends the presence tag and §2.3 descending inverts the whole
    /// component (both at the caller).
    pub fn encode_key(&self) -> Vec<u8> {
        let biased = (self.span() as u128).wrapping_add(1u128 << 127);
        biased.to_be_bytes().to_vec()
    }

    /// `-self` — negate all three fields. `i32::MIN` / `i64::MIN` would overflow → `22008`.
    pub fn neg(&self) -> Result<Interval> {
        Ok(Interval {
            months: self
                .months
                .checked_neg()
                .ok_or_else(|| field_overflow("interval out of range"))?,
            days: self
                .days
                .checked_neg()
                .ok_or_else(|| field_overflow("interval out of range"))?,
            micros: self
                .micros
                .checked_neg()
                .ok_or_else(|| field_overflow("interval out of range"))?,
        })
    }
}

/// gcd of `|a|` and `|b|` (Euclid). Used to reduce a factor fraction to lowest terms so the
/// exact `×÷` cascade does not overflow on a factor like `2.0` (= 20/10 → 2/1).
fn igcd(mut a: i128, mut b: i128) -> i128 {
    a = a.abs();
    b = b.abs();
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// The maximum integer and fractional digits of an `interval × / number` factor — bounds chosen so
/// the exact i128 cascade math cannot overflow (and Go big.Int / TS BigInt agree).
const MAX_INT_DIGITS: usize = 18;
const MAX_FRAC_DIGITS: usize = 9;

/// Parse a canonical decimal string `[-]int[.frac]` into an exact fraction `(num, den)` with
/// `den = 10^len(frac) > 0` (the value = num/den). Caps the digit counts so the i128 cascade
/// stays well-defined; an over-long factor traps `22008` (a documented bound — beyond any real
/// interval factor, and far beyond what PG's `double` represents).
pub fn parse_factor_decimal(s: &str) -> Result<(i128, i128)> {
    let neg = s.starts_with('-');
    let body = s.strip_prefix('-').unwrap_or(s);
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.len() > MAX_INT_DIGITS || frac_part.len() > MAX_FRAC_DIGITS {
        return Err(field_overflow("interval factor has too many digits"));
    }
    let digits: String = format!("{int_part}{frac_part}");
    let mag: i128 = digits
        .parse()
        .map_err(|_| field_overflow("interval factor out of range"))?;
    let num = if neg { -mag } else { mag };
    let den = 10i128.pow(frac_part.len() as u32);
    Ok((num, den))
}

/// The exact `×÷` cascade (spec/design/interval.md §5): multiply each field by `fnum/fden`
/// (`fden > 0`), cascading the fractional part months→days→micros (1 month = 30 days, 1 day =
/// 24 h) with the µs result rounded **half away from zero**. This mirrors PG `interval_mul` /
/// `interval_div` structurally but is EXACT (no float) — a documented PG divergence only on
/// sub-unit ties. All i128 (gcd-reduced first); a field beyond `i32`/`i64`, or an i128 overflow,
/// traps `22008`.
pub fn mul_by_fraction(iv: &Interval, fnum: i128, fden: i128) -> Result<Interval> {
    let g = igcd(fnum, fden).max(1);
    let fnum = fnum / g;
    let fden = fden / g;
    let of = || field_overflow("interval out of range");

    let m = iv.months as i128;
    let d = iv.days as i128;
    let u = iv.micros as i128;

    // result.month = trunc(m * fnum / fden); the fractional months cascade to days.
    let m_total = m.checked_mul(fnum).ok_or_else(of)?;
    let r_month = m_total / fden;
    let frac_month = m_total - r_month * fden; // remainder over fden
    let mrd = frac_month
        .checked_mul(DAYS_PER_MONTH as i128)
        .ok_or_else(of)?; // days·fden
    let mrd_whole = mrd / fden;
    let mrd_frac = mrd - mrd_whole * fden;

    // result.day = trunc(d * fnum / fden) + whole days from the month remainder.
    let d_total = d.checked_mul(fnum).ok_or_else(of)?;
    let r_day_part = d_total / fden;
    let day_frac = d_total - r_day_part * fden;
    let r_day = r_day_part.checked_add(mrd_whole).ok_or_else(of)?;

    // result.time = round( (u·fnum + (day_frac + mrd_frac)·MICROS_PER_DAY) / fden ), half away.
    let time_num = u
        .checked_mul(fnum)
        .and_then(|x| {
            (day_frac + mrd_frac)
                .checked_mul(MICROS_PER_DAY as i128)
                .and_then(|y| x.checked_add(y))
        })
        .ok_or_else(of)?;
    let r_time = round_div(time_num, fden);

    Ok(Interval {
        months: i32::try_from(r_month).map_err(|_| field_overflow("interval out of range"))?,
        days: i32::try_from(r_day).map_err(|_| field_overflow("interval out of range"))?,
        micros: i64::try_from(r_time).map_err(|_| field_overflow("interval out of range"))?,
    })
}

/// `ts + iv` (or `ts - iv` with `subtract`) — the calendar-aware datetime arithmetic
/// (spec/design/interval.md §5, the engine's first timestamp arithmetic). Months are added
/// first **with day-of-month clamping** (Jan 31 + 1 month → Feb 28/29), then days (24 h each —
/// jed has no zones), then microseconds. Adding to ±infinity stays ±infinity (PG); a finite
/// result onto a sentinel or beyond the `i64`-µs range traps `22008`.
pub fn ts_shift(ts: i64, iv: &Interval, subtract: bool) -> Result<i64> {
    if ts == timestamp::NEG_INFINITY || ts == timestamp::POS_INFINITY {
        return Ok(ts); // ±infinity ± any finite interval is unchanged
    }
    let sign: i64 = if subtract { -1 } else { 1 };
    let mut t = ts;

    let months = sign * iv.months as i64;
    if months != 0 {
        let (y, mo, d, h, mi, s, us) = timestamp::civil_from_micros(t);
        // months since (astronomical) year 0, month 0 — floor div/mod handle pre-epoch years.
        let total = y * 12 + (mo as i64 - 1) + months;
        let ny = total.div_euclid(12);
        let nmo = total.rem_euclid(12) as u32 + 1;
        let nd = d.min(timestamp::days_in_month(ny, nmo)); // clamp to the new month's length
        let days = timestamp::days_from_civil(ny, nmo as i64, nd as i64);
        t = days
            .checked_mul(86_400)
            .and_then(|x| x.checked_add(h as i64 * 3600 + mi as i64 * 60 + s as i64))
            .and_then(|secs| secs.checked_mul(MICROS_PER_SEC))
            .and_then(|m| m.checked_add(us as i64))
            .ok_or_else(|| field_overflow("timestamp out of range"))?;
    }

    let day_us = (sign * iv.days as i64)
        .checked_mul(MICROS_PER_DAY)
        .ok_or_else(|| field_overflow("timestamp out of range"))?;
    t = t
        .checked_add(day_us)
        .ok_or_else(|| field_overflow("timestamp out of range"))?;
    t = if subtract {
        t.checked_sub(iv.micros)
    } else {
        t.checked_add(iv.micros)
    }
    .ok_or_else(|| field_overflow("timestamp out of range"))?;

    if t == timestamp::NEG_INFINITY || t == timestamp::POS_INFINITY {
        return Err(field_overflow("timestamp out of range")); // reserved for the sentinels
    }
    Ok(t)
}

/// `a - b` of two timestamps (or two timestamptz) → an interval, justified into days + time with
/// `months = 0` (PG `timestamp_mi` → `interval_justify_hours`: whole 24 h chunks of the µs
/// difference move into `days`). An ±infinity operand traps `22008` (cannot subtract infinite
/// timestamps); a day count beyond `i32` traps `22008`.
pub fn ts_diff(a: i64, b: i64) -> Result<Interval> {
    if a == timestamp::NEG_INFINITY
        || a == timestamp::POS_INFINITY
        || b == timestamp::NEG_INFINITY
        || b == timestamp::POS_INFINITY
    {
        return Err(field_overflow("cannot subtract infinite timestamps"));
    }
    let micros = a
        .checked_sub(b)
        .ok_or_else(|| field_overflow("interval out of range"))?;
    // justify_hours: truncating div/mod give `days` and `micros` the same sign (no fixup needed).
    let days = micros / MICROS_PER_DAY;
    let rem = micros % MICROS_PER_DAY;
    let days = i32::try_from(days).map_err(|_| field_overflow("interval out of range"))?;
    Ok(Interval {
        months: 0,
        days,
        micros: rem,
    })
}

/// PostgreSQL `justify_hours` (spec/design/interval.md §5.1): whole 24 h chunks of `micros` move
/// into `days`, then `days` and `micros` are given the same sign (borrowing one day). `months` is
/// untouched. A day count beyond `i32` traps `22008`.
pub fn justify_hours(iv: &Interval) -> Result<Interval> {
    let whole_day = iv.micros / MICROS_PER_DAY;
    let mut micros = iv.micros - whole_day * MICROS_PER_DAY;
    let mut days = i32::try_from(iv.days as i64 + whole_day)
        .map_err(|_| field_overflow("interval out of range"))?;
    if days > 0 && micros < 0 {
        micros += MICROS_PER_DAY;
        days -= 1;
    } else if days < 0 && micros > 0 {
        micros -= MICROS_PER_DAY;
        days += 1;
    }
    Ok(Interval {
        months: iv.months,
        days,
        micros,
    })
}

/// PostgreSQL `justify_days` (spec/design/interval.md §5.1): whole 30-day chunks of `days` move
/// into `months`, then `months` and `days` are given the same sign (borrowing one month).
/// `micros` is untouched. A month count beyond `i32` traps `22008`.
pub fn justify_days(iv: &Interval) -> Result<Interval> {
    let whole_month = iv.days / DAYS_PER_MONTH as i32;
    let mut days = iv.days - whole_month * DAYS_PER_MONTH as i32;
    let mut months = iv
        .months
        .checked_add(whole_month)
        .ok_or_else(|| field_overflow("interval out of range"))?;
    if months > 0 && days < 0 {
        days += DAYS_PER_MONTH as i32;
        months -= 1;
    } else if months < 0 && days > 0 {
        days -= DAYS_PER_MONTH as i32;
        months += 1;
    }
    Ok(Interval {
        months,
        days,
        micros: iv.micros,
    })
}

/// PostgreSQL `justify_interval` (spec/design/interval.md §5.1): both justifications, with
/// `0 <= |micros| < 24 h`, `0 <= |days| < 30`, and all three fields made the same sign. Ported
/// step for step from PG's `interval_justify_interval`, including its pre-justification of days
/// (which keeps the day sum within `i32`). A month count beyond `i32` traps `22008`.
pub fn justify_interval(iv: &Interval) -> Result<Interval> {
    let oor = || field_overflow("interval out of range");
    let dpm = DAYS_PER_MONTH as i32;
    let mut months = iv.months;
    let mut days = iv.days;
    let mut micros = iv.micros;
    // Pre-justify days when days and micros share a sign (it might prevent overflow).
    if (days > 0 && micros > 0) || (days < 0 && micros < 0) {
        let whole_month = days / dpm;
        days -= whole_month * dpm;
        months = months.checked_add(whole_month).ok_or_else(oor)?;
    }
    let whole_day = micros / MICROS_PER_DAY;
    micros -= whole_day * MICROS_PER_DAY;
    // Cannot overflow: |days| < 30 after the pre-justify, or days and whole_day differ in sign.
    days += whole_day as i32;
    let whole_month = days / dpm;
    days -= whole_month * dpm;
    months = months.checked_add(whole_month).ok_or_else(oor)?;
    if months > 0 && (days < 0 || (days == 0 && micros < 0)) {
        days += dpm;
        months -= 1;
    } else if months < 0 && (days > 0 || (days == 0 && micros > 0)) {
        days -= dpm;
        months += 1;
    }
    if days > 0 && micros < 0 {
        micros += MICROS_PER_DAY;
        days -= 1;
    } else if days < 0 && micros > 0 {
        micros -= MICROS_PER_DAY;
        days += 1;
    }
    Ok(Interval {
        months,
        days,
        micros,
    })
}

/// `age(dt1, dt2)`'s infinity rule (spec/design/interval.md §5.2). PostgreSQL returns an infinite
/// interval for one infinite operand and raises `22008` "interval out of range" for the same
/// infinity twice; jed has no infinite interval, so EVERY infinite operand is that `22008`. Checked
/// before any zone consultation, like PG's `timestamp_age` / `timestamptz_age`.
pub fn age_reject_infinite(dt1: i64, dt2: i64) -> Result<()> {
    let inf = |t: i64| t == timestamp::NEG_INFINITY || t == timestamp::POS_INFINITY;
    if inf(dt1) || inf(dt2) {
        return Err(field_overflow("interval out of range"));
    }
    Ok(())
}

/// PostgreSQL `age(dt1, dt2)` (spec/design/interval.md §5.2), ported step for step from
/// `timestamp_age` / `timestamptz_age`: the field-wise symbolic difference of the two wall clocks
/// `local1` and `local2` (a timestamp's own value; a timestamptz decomposed in the session zone),
/// with the sign taken from comparing the values `dt1` and `dt2` themselves. Each field is
/// subtracted, the result is flipped so the later value comes first, negative fields borrow from
/// the next higher one (a day borrows the length of the EARLIER value's month), and the sign is
/// restored. Both operands are finite (`age_reject_infinite` ran first).
pub fn age(dt1: i64, dt2: i64, local1: i64, local2: i64) -> Result<Interval> {
    let (y1, mo1, d1, h1, mi1, s1, us1) = timestamp::civil_from_micros(local1);
    let (y2, mo2, d2, h2, mi2, s2, us2) = timestamp::civil_from_micros(local2);
    // Form the symbolic difference.
    let mut usec = us1 as i64 - us2 as i64;
    let mut sec = s1 as i64 - s2 as i64;
    let mut min = mi1 as i64 - mi2 as i64;
    let mut hour = h1 as i64 - h2 as i64;
    let mut mday = d1 as i64 - d2 as i64;
    let mut mon = mo1 as i64 - mo2 as i64;
    let mut year = y1 - y2;
    let flip = dt1 < dt2;
    if flip {
        usec = -usec;
        sec = -sec;
        min = -min;
        hour = -hour;
        mday = -mday;
        mon = -mon;
        year = -year;
    }
    // Propagate any negative fields into the next higher field.
    while usec < 0 {
        usec += MICROS_PER_SEC;
        sec -= 1;
    }
    while sec < 0 {
        sec += 60;
        min -= 1;
    }
    while min < 0 {
        min += 60;
        hour -= 1;
    }
    while hour < 0 {
        hour += 24;
        mday -= 1;
    }
    while mday < 0 {
        mday += if flip {
            timestamp::days_in_month(y1, mo1)
        } else {
            timestamp::days_in_month(y2, mo2)
        } as i64;
        mon -= 1;
    }
    while mon < 0 {
        mon += MONTHS_PER_YEAR;
        year -= 1;
    }
    // Recover the sign.
    if flip {
        usec = -usec;
        sec = -sec;
        min = -min;
        hour = -hour;
        mday = -mday;
        mon = -mon;
        year = -year;
    }
    // PG `itm2interval`: the months total must fit i32, the time total i64.
    let oor = || field_overflow("interval out of range");
    let months = year
        .checked_mul(MONTHS_PER_YEAR)
        .and_then(|m| m.checked_add(mon))
        .and_then(|m| i32::try_from(m).ok())
        .ok_or_else(oor)?;
    let days = i32::try_from(mday).map_err(|_| oor())?;
    let micros = hour
        .checked_mul(3600 * MICROS_PER_SEC)
        .and_then(|t| t.checked_add(min * 60 * MICROS_PER_SEC))
        .and_then(|t| t.checked_add(sec * MICROS_PER_SEC))
        .and_then(|t| t.checked_add(usec))
        .ok_or_else(oor)?;
    Ok(Interval {
        months,
        days,
        micros,
    })
}

// Span-canonical equality/ordering/hash (spec/design/interval.md §2): `'1 mon'` and `'30 days'`
// must compare equal and land in the same DISTINCT/GROUP BY bucket, while each keeps its own
// field representation for rendering. Hand-written (not derived) so the canonical span — not the
// field triple — is the identity, mirroring `decimal.rs`.
impl PartialEq for Interval {
    fn eq(&self, other: &Self) -> bool {
        self.span() == other.span()
    }
}
impl Eq for Interval {}
impl std::hash::Hash for Interval {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.span().hash(state);
    }
}
impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Interval {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.span().cmp(&other.span())
    }
}

// --- parsing -----------------------------------------------------------------

fn invalid_format(detail: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::InvalidDatetimeFormat, detail.into())
}

fn field_overflow(detail: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::DatetimeFieldOverflow, detail.into())
}

/// Build an interval from PostgreSQL `make_interval`'s components (spec/design/functions.md §11).
/// `years`/`months` fold into the months field (×12), `weeks`/`days` into the days field (×7),
/// and `hours`/`mins` plus the caller's pre-converted `sec_micros` into the micros field —
/// grouped `(((hours*60)+mins)*60)*1e6 + sec_micros` like PG. All math here is EXACT integer
/// (the one float step, `secs` → `sec_micros`, lives in the executor so this module stays
/// float-free — see the module note). Any i32 month/day or i64 micros overflow traps 22008.
pub fn make_interval(
    years: i64,
    months: i64,
    weeks: i64,
    days: i64,
    hours: i64,
    mins: i64,
    sec_micros: i64,
) -> Result<Interval> {
    let oor = || field_overflow("interval out of range");
    let months_total = years
        .checked_mul(MONTHS_PER_YEAR)
        .and_then(|y| y.checked_add(months))
        .ok_or_else(oor)?;
    let days_total = weeks
        .checked_mul(7)
        .and_then(|w| w.checked_add(days))
        .ok_or_else(oor)?;
    let micros = hours
        .checked_mul(60)
        .and_then(|h| h.checked_add(mins)) // total minutes
        .and_then(|m| m.checked_mul(60)) // total seconds
        .and_then(|s| s.checked_mul(MICROS_PER_SEC))
        .and_then(|t| t.checked_add(sec_micros))
        .ok_or_else(oor)?;
    Ok(Interval {
        months: i32::try_from(months_total).map_err(|_| oor())?,
        days: i32::try_from(days_total).map_err(|_| oor())?,
        micros,
    })
}

/// Round `num / den` to the nearest integer, half away from zero (the engine's one rounding mode
/// — spec/design/decimal.md §3). `den > 0`.
fn round_div(num: i128, den: i128) -> i128 {
    let q = num / den;
    let r = num - q * den;
    let twice = r.abs() * 2;
    if twice >= den {
        if num >= 0 { q + 1 } else { q - 1 }
    } else {
        q
    }
}

// The interval text parser is a port of PostgreSQL 18's `interval_in` (datetime.c): the input is
// broken into fields (`ParseDateTime`), the fields are read right to left (`DecodeInterval`), and
// only if that finds the input malformed is it read as ISO 8601 (`DecodeISO8601Interval`). The one
// numeric difference (spec/design/interval.md §3): PG carries a field's fraction in a `double`;
// jed carries it exactly, as a signed numerator over 10^25 (`FRAC_DEN`), and rounds the µs half
// away from zero (the engine's one mode).

/// PostgreSQL's two datetime-decode failures: `DTERR_BAD_FORMAT` (22007) and
/// `DTERR_FIELD_OVERFLOW` (PG's 22015; jed's datetime field overflow 22008).
#[derive(Clone, Copy, PartialEq, Debug)]
enum DtErr {
    BadFormat,
    FieldOverflow,
}

type Dt<T> = std::result::Result<T, DtErr>;

/// A fraction's denominator: a field's fractional part is `numerator / 10^25`, its digits past the
/// 25th dropped (far below a microsecond for every unit).
const FRAC_DIGITS: usize = 25;
const FRAC_DEN: i128 = 10_000_000_000_000_000_000_000_000;

const USECS_PER_SEC: i64 = MICROS_PER_SEC;
const USECS_PER_MINUTE: i64 = 60 * USECS_PER_SEC;
const USECS_PER_HOUR: i64 = 3600 * USECS_PER_SEC;

/// PG's `struct pg_itm_in`: the fields an interval is assembled from. Years stay apart from months
/// until the end (`itmin2interval`).
#[derive(Default)]
struct ItmIn {
    usec: i64,
    mday: i32,
    mon: i32,
    year: i32,
}

// PG's `DTK_*` unit codes — the values `DecodeInterval`'s `type` carries between fields.
const DTK_NUMBER: i32 = 0; // the `type` after `ago`: no number may precede it
const DTK_TZ: i32 = 4;
const DTK_EARLY: i32 = 9;
const DTK_LATE: i32 = 10;
const IGNORE_DTF: i32 = 8; // the initial `type`: the rightmost unitless number is seconds
const DTK_SECOND: i32 = 18;
const DTK_MINUTE: i32 = 19;
const DTK_HOUR: i32 = 20;
const DTK_DAY: i32 = 21;
const DTK_WEEK: i32 = 22;
const DTK_MONTH: i32 = 23;
const DTK_QUARTER: i32 = 24;
const DTK_YEAR: i32 = 25;
const DTK_DECADE: i32 = 26;
const DTK_CENTURY: i32 = 27;
const DTK_MILLENNIUM: i32 = 28;
const DTK_MILLISEC: i32 = 29;
const DTK_MICROSEC: i32 = 30;
const DTK_JULIAN: i32 = 31;
const DTK_DOW: i32 = 32;
const DTK_DOY: i32 = 33;
const DTK_ISOYEAR: i32 = 36;
const DTK_ISODOW: i32 = 37;

// The per-field masks of `DecodeInterval` (PG `DTK_M`): each field claims its bits, and a field
// whose bits overlap an already-read field's is a duplicate (22007). `week` and `day` are DIFFERENT
// fields, as are `year`/`decade`/`century`/`millennium` and `second`/`millisecond`/`microsecond`.
const FIELD_MONTH: u32 = 1 << 1;
const FIELD_YEAR: u32 = 1 << 2;
const FIELD_DAY: u32 = 1 << 3;
const FIELD_HOUR: u32 = 1 << 10;
const FIELD_MINUTE: u32 = 1 << 11;
const FIELD_SECOND: u32 = 1 << 12;
const FIELD_MILLISECOND: u32 = 1 << 13;
const FIELD_MICROSECOND: u32 = 1 << 14;
const FIELD_WEEK: u32 = 1 << 24;
const FIELD_DECADE: u32 = 1 << 25;
const FIELD_CENTURY: u32 = 1 << 26;
const FIELD_MILLENNIUM: u32 = 1 << 27;
/// Seconds with a fractional part (PG `DTK_ALL_SECS_M`).
const FIELD_ALL_SECS: u32 = FIELD_SECOND | FIELD_MILLISECOND | FIELD_MICROSECOND;
/// A time `HH:MM[:SS[.ffffff]]` (PG `DTK_TIME_M`).
const FIELD_TIME: u32 = FIELD_HOUR | FIELD_MINUTE | FIELD_ALL_SECS;
/// PG `DTK_DATE_M`.
const FIELD_DATE: u32 = FIELD_YEAR | FIELD_MONTH | FIELD_DAY;

/// PG's `MAXDATEFIELDS` and `interval_in`'s 256-byte field buffer: more fields, or more field
/// text, is malformed.
const MAX_DATE_FIELDS: usize = 25;
const FIELD_BUF_LEN: usize = 256;

/// `isspace` in the C locale.
fn is_pg_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// The kind of one field (`ParseDateTime`'s `ftype`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum FieldType {
    /// digits, possibly with one `.` fraction (`5`, `1.5`, `.5`)
    Number,
    /// a lowercased word (`day`, `ago`)
    String,
    /// digits with `-` / `/` / `.` separators, or a word run into digits (`1-2`, `5.`, `p1d`)
    Date,
    /// digits then `:` (`1:00:00.5`)
    Time,
    /// a sign then digits, `:`, `.`, `-` (`-1`, `+02:03`, `-1-2`)
    Tz,
    /// a sign then a word (`-infinity`)
    Special,
}

struct Field {
    ftype: FieldType,
    text: Vec<u8>,
}

/// PostgreSQL's `ParseDateTime`, as `interval_in` uses it: split the input into fields. Whitespace
/// separates fields; any other punctuation starting a field is a separator too; text is lowercased.
fn parse_date_time(input: &[u8]) -> Dt<Vec<Field>> {
    let at = |k: usize| -> u8 { input.get(k).copied().unwrap_or(0) };
    let mut fields: Vec<Field> = Vec::new();
    // Bytes of PG's field buffer used by the finished fields (each field's text plus its NUL).
    let mut buf_used = 0usize;
    let mut i = 0usize;
    while i < input.len() {
        let c = input[i];
        if is_pg_space(c) {
            i += 1;
            continue;
        }
        if fields.len() >= MAX_DATE_FIELDS {
            return Err(DtErr::BadFormat);
        }
        let mut text: Vec<u8> = Vec::new();
        // PG's APPEND_CHAR: the buffer must keep room for the field's terminating NUL.
        let push = |text: &mut Vec<u8>, ch: u8| -> Dt<()> {
            if buf_used + text.len() + 1 >= FIELD_BUF_LEN {
                return Err(DtErr::BadFormat);
            }
            text.push(ch);
            Ok(())
        };
        let ftype;
        if c.is_ascii_digit() {
            while at(i).is_ascii_digit() {
                push(&mut text, at(i))?;
                i += 1;
            }
            if at(i) == b':' {
                // a time field
                ftype = FieldType::Time;
                push(&mut text, b':')?;
                i += 1;
                while at(i).is_ascii_digit() || at(i) == b':' || at(i) == b'.' {
                    push(&mut text, at(i))?;
                    i += 1;
                }
            } else if at(i) == b'-' || at(i) == b'/' || at(i) == b'.' {
                // a date field (or a number with a fraction)
                let delim = at(i);
                push(&mut text, delim)?;
                i += 1;
                if at(i).is_ascii_digit() {
                    let mut t = if delim == b'.' {
                        FieldType::Number
                    } else {
                        FieldType::Date
                    };
                    while at(i).is_ascii_digit() {
                        push(&mut text, at(i))?;
                        i += 1;
                    }
                    // a second matching delimiter makes a three-field date
                    if at(i) == delim {
                        t = FieldType::Date;
                        push(&mut text, delim)?;
                        i += 1;
                        while at(i).is_ascii_digit() || at(i) == delim {
                            push(&mut text, at(i))?;
                            i += 1;
                        }
                    }
                    ftype = t;
                } else {
                    ftype = FieldType::Date;
                    while at(i).is_ascii_alphanumeric() || at(i) == delim {
                        push(&mut text, at(i).to_ascii_lowercase())?;
                        i += 1;
                    }
                }
            } else {
                ftype = FieldType::Number;
            }
        } else if c == b'.' {
            // a leading decimal point: fractional seconds
            push(&mut text, b'.')?;
            i += 1;
            while at(i).is_ascii_digit() {
                push(&mut text, at(i))?;
                i += 1;
            }
            ftype = FieldType::Number;
        } else if c.is_ascii_alphabetic() {
            let mut t = FieldType::String;
            while at(i).is_ascii_alphabetic() {
                push(&mut text, at(i).to_ascii_lowercase())?;
                i += 1;
            }
            // A word may run into a date or zone name — unless it is a known date keyword.
            let is_date = if at(i) == b'-' || at(i) == b'/' || at(i) == b'.' {
                true
            } else if at(i) == b'+' || at(i).is_ascii_digit() {
                !is_date_keyword(&text)
            } else {
                false
            };
            if is_date {
                t = FieldType::Date;
                loop {
                    push(&mut text, at(i).to_ascii_lowercase())?;
                    i += 1;
                    let n = at(i);
                    if !(matches!(n, b'+' | b'-' | b'/' | b'_' | b'.' | b':')
                        || n.is_ascii_alphanumeric())
                    {
                        break;
                    }
                }
            }
            ftype = t;
        } else if c == b'+' || c == b'-' {
            push(&mut text, c)?;
            i += 1;
            // whitespace after the sign is skipped
            while is_pg_space(at(i)) {
                i += 1;
            }
            if at(i).is_ascii_digit() {
                ftype = FieldType::Tz;
                push(&mut text, at(i))?;
                i += 1;
                while at(i).is_ascii_digit() || at(i) == b':' || at(i) == b'.' || at(i) == b'-' {
                    push(&mut text, at(i))?;
                    i += 1;
                }
            } else if at(i).is_ascii_alphabetic() {
                ftype = FieldType::Special;
                while at(i).is_ascii_alphabetic() {
                    push(&mut text, at(i).to_ascii_lowercase())?;
                    i += 1;
                }
            } else {
                return Err(DtErr::BadFormat);
            }
        } else if c.is_ascii_punctuation() {
            // other punctuation only separates fields
            i += 1;
            continue;
        } else {
            return Err(DtErr::BadFormat);
        }
        buf_used += text.len() + 1;
        fields.push(Field { ftype, text });
    }
    Ok(fields)
}

/// Whether a word is in PG's date keyword table (`datetktbl`) — the test that keeps a keyword
/// followed by a digit (`h2`) from being read as a run-on zone name.
fn is_date_keyword(w: &[u8]) -> bool {
    date_keyword(w).is_some()
}

/// What a word means to `DecodeInterval`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Word {
    /// skipped (`at`, `on`)
    Ignore,
    /// a unit: the `DTK_*` code it gives the number to its left
    Units(i32),
    /// `ago`
    Ago,
    /// a reserved word (`infinity`, `epoch`, …): the `DTK_*` code
    Reserv(i32),
    /// a date keyword with no meaning in an interval (a month or weekday name, `am`, …)
    Other,
}

/// PG's interval unit table (`deltatktbl`), then its date keyword table (`datetktbl`) — the lookup
/// `DecodeUnits` then `DecodeSpecial` do. None is an unknown word. PG compares at most 10
/// characters, so any word starting with a 10-letter entry (`millennium`, `microsecon`,
/// `millisecon`) is that unit. jed's one difference: a bare `m` (PG's minute) is ambiguous with
/// month and is rejected.
fn interval_word(w: &[u8]) -> Option<Word> {
    if w == b"m" {
        return None;
    }
    if w.starts_with(b"millennium") {
        return Some(Word::Units(DTK_MILLENNIUM));
    }
    if w.starts_with(b"microsecon") {
        return Some(Word::Units(DTK_MICROSEC));
    }
    if w.starts_with(b"millisecon") {
        return Some(Word::Units(DTK_MILLISEC));
    }
    let unit = match w {
        b"ago" => return Some(Word::Ago),
        b"c" | b"cent" | b"centuries" | b"century" => DTK_CENTURY,
        b"d" | b"day" | b"days" => DTK_DAY,
        b"dec" | b"decade" | b"decades" | b"decs" => DTK_DECADE,
        b"h" | b"hour" | b"hours" | b"hr" | b"hrs" => DTK_HOUR,
        b"mil" | b"millennia" | b"millennium" | b"mils" => DTK_MILLENNIUM,
        b"min" | b"mins" | b"minute" | b"minutes" => DTK_MINUTE,
        b"mon" | b"mons" | b"month" | b"months" => DTK_MONTH,
        b"ms" | b"msec" | b"msecond" | b"mseconds" | b"msecs" => DTK_MILLISEC,
        b"qtr" | b"quarter" => DTK_QUARTER,
        b"s" | b"sec" | b"second" | b"seconds" | b"secs" => DTK_SECOND,
        b"timezone" => DTK_TZ,
        b"us" | b"usec" | b"usecond" | b"useconds" | b"usecs" => DTK_MICROSEC,
        b"w" | b"week" | b"weeks" => DTK_WEEK,
        b"y" | b"year" | b"years" | b"yr" | b"yrs" => DTK_YEAR,
        _ => return date_keyword(w),
    };
    Some(Word::Units(unit))
}

/// PG's date keyword table (`datetktbl`), as an interval reads it.
fn date_keyword(w: &[u8]) -> Option<Word> {
    Some(match w {
        b"at" | b"on" => Word::Ignore,
        b"infinity" | b"+infinity" => Word::Reserv(DTK_LATE),
        b"-infinity" => Word::Reserv(DTK_EARLY),
        b"allballs" | b"epoch" | b"now" | b"today" | b"tomorrow" | b"yesterday" => {
            Word::Reserv(DTK_NUMBER)
        }
        b"d" => Word::Units(DTK_DAY),
        b"dow" => Word::Units(DTK_DOW),
        b"doy" => Word::Units(DTK_DOY),
        b"h" => Word::Units(DTK_HOUR),
        b"isodow" => Word::Units(DTK_ISODOW),
        b"isoyear" => Word::Units(DTK_ISOYEAR),
        b"j" | b"jd" | b"julian" => Word::Units(DTK_JULIAN),
        b"m" => Word::Units(DTK_MONTH),
        b"mm" => Word::Units(DTK_MINUTE),
        b"s" => Word::Units(DTK_SECOND),
        b"y" => Word::Units(DTK_YEAR),
        b"ad" | b"am" | b"apr" | b"april" | b"aug" | b"august" | b"bc" | b"dec" | b"december"
        | b"dst" | b"feb" | b"february" | b"fri" | b"friday" | b"jan" | b"january" | b"jul"
        | b"july" | b"jun" | b"june" | b"mar" | b"march" | b"may" | b"mon" | b"monday" | b"nov"
        | b"november" | b"oct" | b"october" | b"pm" | b"sat" | b"saturday" | b"sep" | b"sept"
        | b"september" | b"sun" | b"sunday" | b"t" | b"thu" | b"thur" | b"thurs" | b"thursday"
        | b"tue" | b"tues" | b"tuesday" | b"wed" | b"wednesday" | b"weds" => Word::Other,
        _ => return None,
    })
}

/// C `strtol` (base 10) as PG's `strtoi64` uses it on a field: an optional sign then digits.
/// Returns the value and the bytes consumed — 0 consumed (value 0) when there are no digits. A
/// value beyond `i64` is `ERANGE`, a field overflow.
fn strtoi64(s: &[u8]) -> Dt<(i64, usize)> {
    let mut i = 0;
    let neg = match s.first() {
        Some(b'-') => {
            i = 1;
            true
        }
        Some(b'+') => {
            i = 1;
            false
        }
        _ => false,
    };
    let start = i;
    let mut v: i128 = 0;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        if !overflow {
            v = v * 10 + (s[i] - b'0') as i128;
            if v > i64::MAX as i128 + 1 {
                overflow = true;
            }
        }
        i += 1;
    }
    if i == start {
        return Ok((0, 0));
    }
    let v = if neg { -v } else { v };
    if overflow || v > i64::MAX as i128 || v < i64::MIN as i128 {
        return Err(DtErr::FieldOverflow);
    }
    Ok((v as i64, i))
}

/// PG's `strtoint`: `strtol` limited to `i32` (beyond it is `ERANGE`, a field overflow).
fn strtoint(s: &[u8]) -> Dt<(i32, usize)> {
    let (v, n) = strtoi64(s)?;
    let v = i32::try_from(v).map_err(|_| DtErr::FieldOverflow)?;
    Ok((v, n))
}

/// The first `FRAC_DIGITS` of a digit run as a fraction numerator over `FRAC_DEN`. A fraction
/// whose nonzero digits all lie past the 25th stays nonzero (numerator 1) — too small to move any
/// field, but still "a fraction" (it decides whether seconds claim the sub-second fields).
fn frac_from_digits(digits: &[u8]) -> i128 {
    let mut num: i128 = 0;
    for k in 0..FRAC_DIGITS {
        num = num * 10 + digits.get(k).map_or(0, |d| (d - b'0') as i128);
    }
    if num == 0 && digits.iter().any(|&d| d != b'0') {
        return 1;
    }
    num
}

/// PG's `ParseFraction`: `s` is `.` then digits (or nothing) and nothing else.
fn parse_fraction(s: &[u8]) -> Dt<i128> {
    let digits = &s[1..];
    if !digits.iter().all(|d| d.is_ascii_digit()) {
        return Err(DtErr::BadFormat);
    }
    Ok(frac_from_digits(digits))
}

/// `ParseFractionalSecond`: a `.digits` fraction of a second to whole µs — 0–6 digits exact, 7+
/// rounded half away from zero on the 7th digit (jed's one mode; PG rounds a `double` half to even).
fn parse_fractional_second(s: &[u8]) -> Dt<i64> {
    let digits = &s[1..];
    if !digits.iter().all(|d| d.is_ascii_digit()) {
        return Err(DtErr::BadFormat);
    }
    let mut us: i64 = 0;
    for k in 0..6 {
        us = us * 10 + digits.get(k).map_or(0, |d| (d - b'0') as i64);
    }
    if digits.len() > 6 && digits[6] >= b'5' {
        us += 1;
    }
    Ok(us)
}

// --- the Adjust* helpers of datetime.c: add a scaled value into the fields, failing on overflow ---

/// `AdjustFractMicroseconds`: `frac * scale` µs, rounded half away from zero.
fn adjust_fract_micros(frac: i128, scale: i64, itm: &mut ItmIn) -> Dt<()> {
    if frac == 0 {
        return Ok(());
    }
    let usec = round_div(frac * scale as i128, FRAC_DEN);
    let usec = i64::try_from(usec).map_err(|_| DtErr::FieldOverflow)?;
    itm.usec = itm.usec.checked_add(usec).ok_or(DtErr::FieldOverflow)?;
    Ok(())
}

/// `AdjustFractDays`: `frac * scale` days — whole days, then the remaining fraction of a day as µs.
fn adjust_fract_days(frac: i128, scale: i64, itm: &mut ItmIn) -> Dt<()> {
    if frac == 0 {
        return Ok(());
    }
    let total = frac * scale as i128;
    let extra_days = total / FRAC_DEN; // truncate toward zero, like C's (int) cast
    itm.mday = itm
        .mday
        .checked_add(extra_days as i32)
        .ok_or(DtErr::FieldOverflow)?;
    adjust_fract_micros(total - extra_days * FRAC_DEN, MICROS_PER_DAY, itm)
}

/// `AdjustFractYears`: `frac * scale` years as whole months, rounded half to even (PG's `rint`);
/// the rest of a month is dropped, as in PG.
fn adjust_fract_years(frac: i128, scale: i64, itm: &mut ItmIn) -> Dt<()> {
    let total = frac * scale as i128 * MONTHS_PER_YEAR as i128;
    let mut extra_months = total / FRAC_DEN;
    let twice_rem = (total - extra_months * FRAC_DEN).abs() * 2;
    if twice_rem > FRAC_DEN || (twice_rem == FRAC_DEN && extra_months % 2 != 0) {
        extra_months += total.signum();
    }
    itm.mon = itm
        .mon
        .checked_add(extra_months as i32)
        .ok_or(DtErr::FieldOverflow)?;
    Ok(())
}

/// `AdjustMicroseconds`: `(val + frac) * scale` µs.
fn adjust_micros(val: i64, frac: i128, scale: i64, itm: &mut ItmIn) -> Dt<()> {
    itm.usec = val
        .checked_mul(scale)
        .and_then(|p| itm.usec.checked_add(p))
        .ok_or(DtErr::FieldOverflow)?;
    adjust_fract_micros(frac, scale, itm)
}

/// `AdjustDays`: `val * scale` days.
fn adjust_days(val: i64, scale: i32, itm: &mut ItmIn) -> Dt<()> {
    let v = i32::try_from(val).map_err(|_| DtErr::FieldOverflow)?;
    itm.mday = v
        .checked_mul(scale)
        .and_then(|d| itm.mday.checked_add(d))
        .ok_or(DtErr::FieldOverflow)?;
    Ok(())
}

/// `AdjustMonths`: `val` months.
fn adjust_months(val: i64, itm: &mut ItmIn) -> Dt<()> {
    let v = i32::try_from(val).map_err(|_| DtErr::FieldOverflow)?;
    itm.mon = itm.mon.checked_add(v).ok_or(DtErr::FieldOverflow)?;
    Ok(())
}

/// `AdjustYears`: `val * scale` years.
fn adjust_years(val: i64, scale: i32, itm: &mut ItmIn) -> Dt<()> {
    let v = i32::try_from(val).map_err(|_| DtErr::FieldOverflow)?;
    itm.year = v
        .checked_mul(scale)
        .and_then(|y| itm.year.checked_add(y))
        .ok_or(DtErr::FieldOverflow)?;
    Ok(())
}

/// PG's `DecodeTimeForInterval` (`DecodeTimeCommon`): a time field `H:M`, `H:M:S[.f]`, or
/// `M:S.f` (two parts with a fraction are minutes and seconds). The time REPLACES `itm.usec`, as
/// in PG — so a fractional unit to the time's left (read earlier) loses its µs part.
fn decode_time_for_interval(s: &[u8], mm_ss: bool, itm: &mut ItmIn) -> Dt<()> {
    let (mut hour, n) = strtoi64(s)?;
    let mut cp = &s[n..];
    if cp.first() != Some(&b':') {
        return Err(DtErr::BadFormat);
    }
    let (mut min, n) = strtoint(&cp[1..])?;
    cp = &cp[1 + n..];
    let mut sec: i32 = 0;
    let mut fsec: i64 = 0;
    match cp.first() {
        // `a:b` under `MINUTE TO SECOND` is minutes:seconds (PG `DecodeTimeCommon`).
        None if mm_ss => {
            if i32::try_from(hour).is_err() {
                return Err(DtErr::FieldOverflow);
            }
            sec = min;
            min = hour as i32;
            hour = 0;
        }
        None => {}
        Some(b'.') => {
            // mm:ss.fff
            fsec = parse_fractional_second(cp)?;
            if i32::try_from(hour).is_err() {
                return Err(DtErr::FieldOverflow);
            }
            sec = min;
            min = hour as i32;
            hour = 0;
        }
        Some(b':') => {
            let (s2, n) = strtoint(&cp[1..])?;
            sec = s2;
            cp = &cp[1 + n..];
            match cp.first() {
                None => {}
                Some(b'.') => fsec = parse_fractional_second(cp)?,
                Some(_) => return Err(DtErr::BadFormat),
            }
        }
        Some(_) => return Err(DtErr::BadFormat),
    }
    if hour < 0 || !(0..=59).contains(&min) || !(0..=60).contains(&sec) || fsec > USECS_PER_SEC {
        return Err(DtErr::FieldOverflow);
    }
    itm.usec = hour
        .checked_mul(USECS_PER_HOUR)
        .and_then(|h| h.checked_add(min as i64 * USECS_PER_MINUTE))
        .and_then(|hm| hm.checked_add(sec as i64 * USECS_PER_SEC))
        .and_then(|hms| hms.checked_add(fsec))
        .ok_or(DtErr::FieldOverflow)?;
    Ok(())
}

/// PG's `DecodeInterval` (IntervalStyle `postgres`, no field qualifier): read the fields right to
/// left, so a unit word is met before the number it applies to. A number with no unit takes the
/// last unit read: seconds at the right end, then whatever unit was read last — except that after an
/// `hour` or a time field the next unitless number is days (`'1 1:00'`, `'5 1 hour'`). `N-M` is
/// years-months. A field qualifier (`range`, its datetime_units.toml code, 0 for none) gives the
/// rightmost unitless number its least field's unit (`'5' DAY` is 5 days) and makes `a:b` read as
/// mm:ss under `MINUTE TO SECOND` (interval.md §6.1). Returns whether the input was `infinity` /
/// `-infinity`.
fn decode_interval(fields: &[Field], range: u8, itm: &mut ItmIn) -> Dt<bool> {
    let qual = qualifier(range);
    let mm_ss = qual.is_some_and(|q| q.mm_ss);
    let mut is_before = false;
    let mut parsing_unit_val = false;
    let mut infinite = false;
    let mut fmask: u32 = 0;
    let mut ty = IGNORE_DTF;
    let nf = fields.len();
    for i in (0..nf).rev() {
        let f = &fields[i];
        let mut tmask: u32 = 0;
        let mut ftype = f.ftype;
        if ftype == FieldType::Time {
            decode_time_for_interval(&f.text, mm_ss, itm)?;
            tmask = FIELD_TIME;
            ty = DTK_DAY;
            parsing_unit_val = false;
        } else if ftype == FieldType::Tz {
            // A signed time (`-1:00`, `+02:03`) — else a signed number, read below.
            if f.text[1..].contains(&b':')
                && decode_time_for_interval(&f.text[1..], mm_ss, itm).is_ok()
            {
                if f.text[0] == b'-' {
                    itm.usec = itm.usec.checked_neg().ok_or(DtErr::FieldOverflow)?;
                }
                tmask = FIELD_TIME;
                ty = DTK_DAY;
                parsing_unit_val = false;
            } else {
                ftype = FieldType::Number;
            }
        }
        match ftype {
            FieldType::Time | FieldType::Tz => {}
            FieldType::Number | FieldType::Date => {
                if ty == IGNORE_DTF {
                    // the qualifier's least field decides what the rightmost number is
                    ty = match qual.map(|q| q.least) {
                        Some("year") => DTK_YEAR,
                        Some("month") => DTK_MONTH,
                        Some("day") => DTK_DAY,
                        Some("hour") => DTK_HOUR,
                        Some("minute") => DTK_MINUTE,
                        _ => DTK_SECOND,
                    };
                }
                let s = &f.text[..];
                let (mut val, n) = strtoi64(s)?;
                let cp = &s[n..];
                let negative = s[0] == b'-';
                let mut fval: i128 = 0;
                match cp.first() {
                    Some(b'-') => {
                        // SQL-standard years-months
                        let (v2, n2) = strtoint(&cp[1..])?;
                        if !(0..MONTHS_PER_YEAR as i32).contains(&v2) {
                            return Err(DtErr::FieldOverflow);
                        }
                        if cp.len() != 1 + n2 {
                            return Err(DtErr::BadFormat);
                        }
                        ty = DTK_MONTH;
                        let v2 = if negative { -v2 } else { v2 };
                        val = val
                            .checked_mul(MONTHS_PER_YEAR)
                            .and_then(|m| m.checked_add(v2 as i64))
                            .ok_or(DtErr::FieldOverflow)?;
                    }
                    Some(b'.') => {
                        fval = parse_fraction(cp)?;
                        if negative {
                            fval = -fval;
                        }
                    }
                    None => {}
                    Some(_) => return Err(DtErr::BadFormat),
                }
                match ty {
                    DTK_MICROSEC => {
                        adjust_micros(val, fval, 1, itm)?;
                        tmask = FIELD_MICROSECOND;
                    }
                    DTK_MILLISEC => {
                        adjust_micros(val, fval, 1000, itm)?;
                        tmask = FIELD_MILLISECOND;
                    }
                    DTK_SECOND => {
                        adjust_micros(val, fval, USECS_PER_SEC, itm)?;
                        // fractional seconds claim the millisecond and microsecond fields too
                        tmask = if fval == 0 {
                            FIELD_SECOND
                        } else {
                            FIELD_ALL_SECS
                        };
                    }
                    DTK_MINUTE => {
                        adjust_micros(val, fval, USECS_PER_MINUTE, itm)?;
                        tmask = FIELD_MINUTE;
                    }
                    DTK_HOUR => {
                        adjust_micros(val, fval, USECS_PER_HOUR, itm)?;
                        tmask = FIELD_HOUR;
                        ty = DTK_DAY; // a unitless number left of an hour is days
                    }
                    DTK_DAY => {
                        adjust_days(val, 1, itm)?;
                        adjust_fract_micros(fval, MICROS_PER_DAY, itm)?;
                        tmask = FIELD_DAY;
                    }
                    DTK_WEEK => {
                        adjust_days(val, 7, itm)?;
                        adjust_fract_days(fval, 7, itm)?;
                        tmask = FIELD_WEEK;
                    }
                    DTK_MONTH => {
                        adjust_months(val, itm)?;
                        adjust_fract_days(fval, DAYS_PER_MONTH, itm)?;
                        tmask = FIELD_MONTH;
                    }
                    DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM => {
                        let (scale, mask) = match ty {
                            DTK_YEAR => (1, FIELD_YEAR),
                            DTK_DECADE => (10, FIELD_DECADE),
                            DTK_CENTURY => (100, FIELD_CENTURY),
                            _ => (1000, FIELD_MILLENNIUM),
                        };
                        adjust_years(val, scale, itm)?;
                        adjust_fract_years(fval, scale as i64, itm)?;
                        tmask = mask;
                    }
                    _ => return Err(DtErr::BadFormat),
                }
                parsing_unit_val = false;
            }
            FieldType::String | FieldType::Special => {
                // two unit words in a row
                if parsing_unit_val {
                    return Err(DtErr::BadFormat);
                }
                match interval_word(&f.text) {
                    Some(Word::Ignore) => {
                        // skipped — but, as in PG, it resets the unit a number to its left takes
                        ty = IGNORE_DTF;
                        continue;
                    }
                    Some(Word::Units(u)) => {
                        ty = u;
                        parsing_unit_val = true;
                    }
                    Some(Word::Ago) => {
                        // `ago` only as the last field
                        if i != nf - 1 {
                            return Err(DtErr::BadFormat);
                        }
                        is_before = true;
                        ty = DTK_NUMBER;
                    }
                    Some(Word::Reserv(r)) => {
                        tmask = FIELD_DATE | FIELD_TIME;
                        // only `infinity` / `-infinity`, and nothing after it
                        if (r != DTK_LATE && r != DTK_EARLY) || i != nf - 1 {
                            return Err(DtErr::BadFormat);
                        }
                        infinite = true;
                    }
                    Some(Word::Other) | None => return Err(DtErr::BadFormat),
                }
            }
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }
    // at least one field, and no unit left without its number
    if fmask == 0 || parsing_unit_val {
        return Err(DtErr::BadFormat);
    }
    if is_before {
        itm.usec = itm.usec.checked_neg().ok_or(DtErr::FieldOverflow)?;
        itm.mday = itm.mday.checked_neg().ok_or(DtErr::FieldOverflow)?;
        itm.mon = itm.mon.checked_neg().ok_or(DtErr::FieldOverflow)?;
        itm.year = itm.year.checked_neg().ok_or(DtErr::FieldOverflow)?;
    }
    Ok(infinite)
}

// `strtod`'s range limits, to 40 significant digits: a decimal at or above the overflow threshold
// (2^1024 − 2^970, ~1.8e308) rounds to infinity, and a nonzero one below the underflow threshold
// (2^-1022 − 2^-1076, ~2.2e-308) is too small for a normal double — both `ERANGE`, which PG
// reports as malformed input.
const STRTOD_OVERFLOW_DIGITS: &[u8; 40] = b"1797693134862315807937289714053034150799";
const STRTOD_UNDERFLOW_DIGITS: &[u8; 40] = b"2225073858507201259573821257020768020077";

/// Compare a significand's leading 40 digits (zero-padded) with a 40-digit threshold.
fn cmp_lead_digits(d: &[u8], t: &[u8; 40]) -> std::cmp::Ordering {
    for (k, &tk) in t.iter().enumerate() {
        let dk = d.get(k).copied().unwrap_or(b'0');
        if dk != tk {
            return dk.cmp(&tk);
        }
    }
    std::cmp::Ordering::Equal
}

/// Read a run of decimal digits as an exponent, saturating far beyond any meaningful value.
fn read_exponent(s: &[u8], i: &mut usize) -> i64 {
    let mut e: i64 = 0;
    while *i < s.len() && s[*i].is_ascii_digit() {
        e = (e * 10 + (s[*i] - b'0') as i64).min(1_000_000_000_000);
        *i += 1;
    }
    e
}

/// The signed-exponent tail `[eE|pP][+-]digits` of a `strtod` number; only consumed when digits
/// follow.
fn read_exponent_part(s: &[u8], i: &mut usize, marker: u8) -> i64 {
    let at = |k: usize| -> u8 { s.get(k).copied().unwrap_or(0) };
    if at(*i).to_ascii_lowercase() != marker {
        return 0;
    }
    let mut j = *i + 1;
    let mut neg = false;
    if at(j) == b'+' || at(j) == b'-' {
        neg = at(j) == b'-';
        j += 1;
    }
    if !at(j).is_ascii_digit() {
        return 0;
    }
    let e = read_exponent(s, &mut j);
    *i = j;
    if neg { -e } else { e }
}

/// A decimal `strtod` number `digits[.digits][e[+-]digits]` at `s[i..]` (no sign) as
/// `(integer part, fraction numerator over FRAC_DEN, end)`, or the `ParseISO8601Number` error: a
/// value `strtod` would report `ERANGE` for is malformed, one beyond ±1e15 a field overflow.
fn scan_decimal(s: &[u8], mut i: usize) -> Dt<(i64, i128, usize)> {
    let mut digits: Vec<u8> = Vec::new();
    let mut frac_count: i64 = 0;
    let mut any = false;
    while i < s.len() && s[i].is_ascii_digit() {
        digits.push(s[i]);
        any = true;
        i += 1;
    }
    if i < s.len() && s[i] == b'.' && (any || s.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
        i += 1;
        while i < s.len() && s[i].is_ascii_digit() {
            digits.push(s[i]);
            frac_count += 1;
            any = true;
            i += 1;
        }
    }
    if !any {
        return Err(DtErr::BadFormat);
    }
    // value = significand * 10^exp, the significand without leading or trailing zeros
    let mut exp = read_exponent_part(s, &mut i, b'e') - frac_count;
    let first = digits.iter().position(|&d| d != b'0');
    let Some(first) = first else {
        return Ok((0, 0, i));
    };
    let mut sig: Vec<u8> = digits[first..].to_vec();
    while sig.last() == Some(&b'0') {
        sig.pop();
        exp += 1;
    }
    // the power of ten of the leading digit
    let lead = sig.len() as i64 - 1 + exp;
    if lead > 308 || (lead == 308 && cmp_lead_digits(&sig, STRTOD_OVERFLOW_DIGITS).is_gt()) {
        return Err(DtErr::BadFormat);
    }
    if lead < -308 || (lead == -308 && cmp_lead_digits(&sig, STRTOD_UNDERFLOW_DIGITS).is_le()) {
        return Err(DtErr::BadFormat);
    }
    if lead > 15 || (lead == 15 && sig != b"1") {
        return Err(DtErr::FieldOverflow);
    }
    if exp >= 0 {
        let mut v: i64 = 0;
        for &d in &sig {
            v = v * 10 + (d - b'0') as i64;
        }
        return Ok((v * 10i64.pow(exp as u32), 0, i));
    }
    // `k` digits are after the decimal point
    let k = (-exp) as usize;
    let (int_digits, frac_digits): (&[u8], Vec<u8>) = if sig.len() > k {
        (&sig[..sig.len() - k], sig[sig.len() - k..].to_vec())
    } else {
        let zeros = (k - sig.len()).min(FRAC_DIGITS);
        let mut fd = vec![b'0'; zeros];
        fd.extend_from_slice(&sig);
        (&[], fd)
    };
    let mut v: i64 = 0;
    for &d in int_digits {
        v = v * 10 + (d - b'0') as i64;
    }
    Ok((v, frac_from_digits(&frac_digits), i))
}

/// A hexadecimal `strtod` number `0x` `hex[.hex][p[+-]digits]` whose digits start at `s[i..]` —
/// as [`scan_decimal`], computed exactly from its bits (the fraction to 64 bits).
fn scan_hex(s: &[u8], mut i: usize) -> Dt<(i64, i128, usize)> {
    let mut bits: Vec<u8> = Vec::new();
    let mut frac_hex: i64 = 0;
    let push_hex = |bits: &mut Vec<u8>, h: u8| {
        let v = (h as char).to_digit(16).unwrap_or(0) as u8;
        for b in (0..4).rev() {
            bits.push((v >> b) & 1);
        }
    };
    while i < s.len() && s[i].is_ascii_hexdigit() {
        push_hex(&mut bits, s[i]);
        i += 1;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_hexdigit() {
            push_hex(&mut bits, s[i]);
            frac_hex += 1;
            i += 1;
        }
    }
    // value = bits * 2^e2
    let e2 = read_exponent_part(s, &mut i, b'p') - 4 * frac_hex;
    let Some(first) = bits.iter().position(|&b| b == 1) else {
        return Ok((0, 0, i));
    };
    let bits = &bits[first..];
    let len = bits.len() as i64;
    // the power of two of the leading bit, and of the lowest set bit
    let lead = len - 1 + e2;
    let last = bits.iter().rposition(|&b| b == 1).unwrap_or(0) as i64;
    let low = len - 1 - last + e2;
    // the leading 54 bits all ones: rounds up to the next power of two in 53-bit precision
    let rounds_up = bits.len() >= 54 && bits[..54].iter().all(|&b| b == 1);
    if lead > 1023 || (lead == 1023 && rounds_up) {
        return Err(DtErr::BadFormat);
    }
    // ERANGE on underflow: below the normal range and not exact in the subnormal range
    let tiny = lead < -1022 && !(lead == -1023 && rounds_up);
    if tiny && low < -1074 {
        return Err(DtErr::BadFormat);
    }
    if lead >= 50 {
        return Err(DtErr::FieldOverflow);
    }
    let bit_at = |pow: i64| -> i128 {
        // the bit worth 2^pow
        let p = lead - pow;
        if p >= 0 && p < len {
            bits[p as usize] as i128
        } else {
            0
        }
    };
    let mut ipart: i128 = 0;
    for pow in (0..=lead).rev() {
        ipart = ipart * 2 + bit_at(pow);
    }
    let mut f64bits: i128 = 0;
    for q in 1..=64 {
        f64bits = f64bits * 2 + bit_at(-q);
    }
    if ipart > 1_000_000_000_000_000 || (ipart == 1_000_000_000_000_000 && f64bits > 0) {
        return Err(DtErr::FieldOverflow);
    }
    // f64bits / 2^64 = f64bits * 5^25 / 2^39 / 10^25
    let frac = (f64bits * 298_023_223_876_953_125) >> 39;
    Ok((ipart as i64, frac, i))
}

/// PG's `ParseISO8601Number`: a `strtod` number at `s[i..]` (it must start with a digit, `-`, or
/// `.`) as `(integer part, fraction, end)`, both parts carrying the sign.
fn parse_iso8601_number(s: &[u8], i: usize) -> Dt<(i64, i128, usize)> {
    let at = |k: usize| -> u8 { s.get(k).copied().unwrap_or(0) };
    let c = at(i);
    if !(c.is_ascii_digit() || c == b'-' || c == b'.') {
        return Err(DtErr::BadFormat);
    }
    let neg = c == b'-';
    let j = if neg { i + 1 } else { i };
    // strtod's infinity / NaN spellings: a value out of range
    let word = |w: &[u8]| (0..w.len()).all(|k| at(j + k).to_ascii_lowercase() == w[k]);
    if word(b"inf") || word(b"nan") {
        return Err(DtErr::FieldOverflow);
    }
    let is_hex = at(j) == b'0'
        && at(j + 1).to_ascii_lowercase() == b'x'
        && (at(j + 2).is_ascii_hexdigit() || (at(j + 2) == b'.' && at(j + 3).is_ascii_hexdigit()));
    let (ipart, frac, end) = if is_hex {
        scan_hex(s, j + 2)?
    } else {
        scan_decimal(s, j)?
    };
    if neg {
        Ok((-ipart, -frac, end))
    } else {
        Ok((ipart, frac, end))
    }
}

/// PG's `ISO8601IntegerWidth`: the integer digits of a number field, past a leading `-`.
fn iso8601_integer_width(s: &[u8]) -> usize {
    let s = if s.first() == Some(&b'-') { &s[1..] } else { s };
    s.iter().take_while(|d| d.is_ascii_digit()).count()
}

/// PG's `DecodeISO8601Interval`: the ISO 8601 "format with designators" (`P1Y2M3DT4H5M6S`, `P2W`,
/// any field fractional or negative) and the "alternative format" (`P0001-02-03T04:05:06`, basic
/// `P00010203T040506`). The whole input, untrimmed, must be the value.
fn decode_iso8601_interval(s: &[u8], itm: &mut ItmIn) -> Dt<()> {
    *itm = ItmIn::default();
    if s.len() < 2 || s[0] != b'P' {
        return Err(DtErr::BadFormat);
    }
    let at = |k: usize| -> u8 { s.get(k).copied().unwrap_or(0) };
    let mut i = 1usize;
    let mut datepart = true;
    let mut havefield = false;
    while i < s.len() {
        if s[i] == b'T' {
            // the start of the time part
            datepart = false;
            havefield = false;
            i += 1;
            continue;
        }
        let fieldstart = i;
        let (val, fval, end) = parse_iso8601_number(s, i)?;
        i = end;
        let unit = at(i);
        i += 1;
        if datepart {
            match unit {
                b'Y' => {
                    adjust_years(val, 1, itm)?;
                    adjust_fract_years(fval, 1, itm)?;
                }
                b'M' => {
                    adjust_months(val, itm)?;
                    adjust_fract_days(fval, DAYS_PER_MONTH, itm)?;
                }
                b'W' => {
                    adjust_days(val, 7, itm)?;
                    adjust_fract_days(fval, 7, itm)?;
                }
                b'D' => {
                    adjust_days(val, 1, itm)?;
                    adjust_fract_micros(fval, MICROS_PER_DAY, itm)?;
                }
                b'T' | 0 | b'-' => {
                    if unit != b'-' && iso8601_integer_width(&s[fieldstart..]) == 8 && !havefield {
                        // alternative format, basic: YYYYMMDD
                        adjust_years(val / 10000, 1, itm)?;
                        adjust_months((val / 100) % 100, itm)?;
                        adjust_days(val % 100, 1, itm)?;
                        adjust_fract_micros(fval, MICROS_PER_DAY, itm)?;
                        if unit == 0 {
                            return Ok(());
                        }
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    // alternative format, extended: YYYY-MM-DD
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    adjust_years(val, 1, itm)?;
                    adjust_fract_years(fval, 1, itm)?;
                    if unit == 0 {
                        return Ok(());
                    }
                    if unit == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    let (val, fval, end) = parse_iso8601_number(s, i)?;
                    i = end;
                    adjust_months(val, itm)?;
                    adjust_fract_days(fval, DAYS_PER_MONTH, itm)?;
                    if i >= s.len() {
                        return Ok(());
                    }
                    if s[i] == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    if s[i] != b'-' {
                        return Err(DtErr::BadFormat);
                    }
                    i += 1;
                    let (val, fval, end) = parse_iso8601_number(s, i)?;
                    i = end;
                    adjust_days(val, 1, itm)?;
                    adjust_fract_micros(fval, MICROS_PER_DAY, itm)?;
                    if i >= s.len() {
                        return Ok(());
                    }
                    if s[i] == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        } else {
            match unit {
                b'H' => adjust_micros(val, fval, USECS_PER_HOUR, itm)?,
                b'M' => adjust_micros(val, fval, USECS_PER_MINUTE, itm)?,
                b'S' => adjust_micros(val, fval, USECS_PER_SEC, itm)?,
                0 | b':' => {
                    if unit == 0 && iso8601_integer_width(&s[fieldstart..]) == 6 && !havefield {
                        // alternative format, basic: HHMMSS
                        adjust_micros(val / 10000, 0, USECS_PER_HOUR, itm)?;
                        adjust_micros((val / 100) % 100, 0, USECS_PER_MINUTE, itm)?;
                        adjust_micros(val % 100, 0, USECS_PER_SEC, itm)?;
                        adjust_fract_micros(fval, 1, itm)?;
                        return Ok(());
                    }
                    // alternative format, extended: HH:MM:SS
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    adjust_micros(val, fval, USECS_PER_HOUR, itm)?;
                    if unit == 0 {
                        return Ok(());
                    }
                    let (val, fval, end) = parse_iso8601_number(s, i)?;
                    i = end;
                    adjust_micros(val, fval, USECS_PER_MINUTE, itm)?;
                    if i >= s.len() {
                        return Ok(());
                    }
                    if s[i] != b':' {
                        return Err(DtErr::BadFormat);
                    }
                    i += 1;
                    let (val, fval, end) = parse_iso8601_number(s, i)?;
                    i = end;
                    adjust_micros(val, fval, USECS_PER_SEC, itm)?;
                    if i >= s.len() {
                        return Ok(());
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        }
        havefield = true;
    }
    Ok(())
}

/// Parse interval text into the three-field value — PostgreSQL's `interval_in`
/// (spec/design/interval.md §3): the PostgreSQL / SQL-standard field syntax, else ISO 8601.
/// Malformed input is 22007; a field beyond its range is 22008 (PG's 22015).
pub fn parse_interval(input: &str) -> Result<Interval> {
    parse_interval_ranged(input, 0)
}

/// Parse interval text for a target carrying a type modifier — PostgreSQL's `interval_in` with a
/// typmod (interval.md §6.1): the qualifier steers the parse, then the value is cut to it and
/// rounded to its precision ([`adjust_interval_for_typmod`]). `None` is plain [`parse_interval`].
pub fn parse_interval_typmod(
    input: &str,
    typmod: Option<crate::types::TemporalTypmod>,
) -> Result<Interval> {
    match typmod {
        None => parse_interval(input),
        Some(tm) => adjust_interval_for_typmod(parse_interval_ranged(input, tm.fields)?, tm),
    }
}

fn parse_interval_ranged(input: &str, range: u8) -> Result<Interval> {
    let b = input.as_bytes();
    let mut itm = ItmIn::default();
    let mut res = parse_date_time(b).and_then(|fields| decode_interval(&fields, range, &mut itm));
    if res == Err(DtErr::BadFormat) {
        res = decode_iso8601_interval(b, &mut itm).map(|()| false);
    }
    match res {
        Err(DtErr::BadFormat) | Ok(true) => {
            // (jed has no infinite interval, so `infinity` is not an interval value)
            Err(invalid_format(format!(
                "invalid input syntax for type interval: \"{input}\""
            )))
        }
        Err(DtErr::FieldOverflow) => Err(field_overflow(format!(
            "interval field value out of range: \"{input}\""
        ))),
        Ok(false) => {
            let months = itm.year as i64 * MONTHS_PER_YEAR + itm.mon as i64;
            Ok(Interval {
                months: i32::try_from(months)
                    .map_err(|_| field_overflow("interval out of range"))?,
                days: itm.mday,
                micros: itm.usec,
            })
        }
    }
}

// --- type modifiers (interval.md §6.1) ----------------------------------------

/// The largest interval precision (PostgreSQL's `MAX_INTERVAL_PRECISION`); a larger declared
/// precision is reduced to it.
pub const MAX_INTERVAL_PRECISION: u8 = 6;

/// The interval field qualifier with stable `code` (spec/types/datetime_units.toml), or `None` for
/// code 0 (no qualifier) or an unknown code.
pub fn qualifier(code: u8) -> Option<&'static IntervalQualifier> {
    INTERVAL_QUALIFIERS.iter().find(|q| q.code == code)
}

/// The qualifier written as `name` (`"day to second"`), if it is one of the thirteen.
pub fn qualifier_by_name(name: &str) -> Option<&'static IntervalQualifier> {
    INTERVAL_QUALIFIERS.iter().find(|q| q.name == name)
}

/// Cut an interval to a type modifier — PostgreSQL's `AdjustIntervalForTypmod`: the fields below
/// the qualifier's least field go (YEAR keeps whole years of the months and zeroes days and time;
/// MONTH zeroes days and time; DAY zeroes the time; HOUR and MINUTE truncate the time toward zero —
/// higher fields are kept), then the time rounds half away from zero to the precision. A rounding
/// that overflows is 22008.
pub fn adjust_interval_for_typmod(
    iv: Interval,
    typmod: crate::types::TemporalTypmod,
) -> Result<Interval> {
    let mut out = iv;
    match qualifier(typmod.fields).map(|q| q.least) {
        Some("year") => {
            out.months = out.months / MONTHS_PER_YEAR as i32 * MONTHS_PER_YEAR as i32;
            out.days = 0;
            out.micros = 0;
        }
        Some("month") => {
            out.days = 0;
            out.micros = 0;
        }
        Some("day") => out.micros = 0,
        Some("hour") => out.micros = out.micros / USECS_PER_HOUR * USECS_PER_HOUR,
        Some("minute") => out.micros = out.micros / USECS_PER_MINUTE * USECS_PER_MINUTE,
        _ => {}
    }
    if let Some(p) = typmod.precision
        && p < MAX_INTERVAL_PRECISION
    {
        let scale = 10i64.pow(u32::from(MAX_INTERVAL_PRECISION - p));
        let offset = scale / 2;
        let shifted = if out.micros >= 0 {
            out.micros.checked_add(offset)
        } else {
            out.micros.checked_sub(offset)
        }
        .ok_or_else(|| field_overflow("interval out of range"))?;
        out.micros = shifted - shifted % scale;
    }
    Ok(out)
}

// --- rendering ---------------------------------------------------------------

/// Render an interval to PostgreSQL's canonical `IntervalStyle = postgres` text
/// (spec/design/interval.md §4): `1 year 2 mons 3 days 04:05:06`, `-00:00:01`, `1 day 12:00:00`,
/// with a bare `00:00:00` for the zero interval. Pure integer→string formatting (no locale).
pub fn render_interval(iv: &Interval) -> String {
    let months = iv.months as i64;
    let days = iv.days as i64;
    let micros = iv.micros;
    if months == 0 && days == 0 && micros == 0 {
        return "00:00:00".to_string();
    }
    let year = months / MONTHS_PER_YEAR;
    let mon = months % MONTHS_PER_YEAR;

    let mut out = String::new();
    let mut is_zero = true;
    let mut is_before = false;
    add_int_part(&mut out, year, "year", &mut is_zero, &mut is_before);
    add_int_part(&mut out, mon, "mon", &mut is_zero, &mut is_before);
    add_int_part(&mut out, days, "day", &mut is_zero, &mut is_before);

    if micros != 0 || is_zero {
        let neg = micros < 0;
        let a = (micros as i128).unsigned_abs();
        // The micros field is NOT justified into days, so the hour count is unbounded
        // (`INTERVAL '100 hours'` renders `100:00:00`).
        let h = a / (3600 * MICROS_PER_SEC as u128);
        let mi = (a / (60 * MICROS_PER_SEC as u128)) % 60;
        let s = (a / MICROS_PER_SEC as u128) % 60;
        let us = a % MICROS_PER_SEC as u128;
        if !is_zero {
            out.push(' ');
        }
        out.push_str(if neg {
            "-"
        } else if is_before {
            "+"
        } else {
            ""
        });
        out.push_str(&format!("{h:02}:{mi:02}:{s:02}"));
        if us != 0 {
            let frac = format!("{us:06}");
            out.push('.');
            out.push_str(frac.trim_end_matches('0'));
        }
    }
    out
}

/// Append one integer field (year/mon/day) in PostgreSQL postgres-style: nothing when zero;
/// otherwise a leading space (unless first), a `+` only when a previous field was negative and
/// this one is positive, the value, the unit, and a plural `s` when the value is not exactly 1.
fn add_int_part(
    out: &mut String,
    value: i64,
    unit: &str,
    is_zero: &mut bool,
    is_before: &mut bool,
) {
    if value == 0 {
        return;
    }
    if !*is_zero {
        out.push(' ');
    }
    if *is_before && value > 0 {
        out.push('+');
    }
    out.push_str(&value.to_string());
    out.push(' ');
    out.push_str(unit);
    if value != 1 {
        out.push('s');
    }
    *is_before = value < 0;
    *is_zero = false;
}
