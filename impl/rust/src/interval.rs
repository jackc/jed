//! The `interval` type — value model, parsing, and rendering (spec/design/interval.md).
//!
//! A value is PostgreSQL's three independent fields: `months` (i32), `days` (i32), and
//! `micros` (i64). They are kept separate so `+ 1 month` is calendar-aware (and distinct from
//! `+ 30 days`); comparison/ordering/dedup collapse them via the canonical 128-bit **span**
//! (1 month = 30 days, 1 day = 24 h). Parsing accepts the "unit + time" subset (units with
//! abbreviations, per-field signs, trailing `ago`, the fractional-unit cascade, and a bare
//! `HH:MM:SS[.ffffff]` time); ISO-8601 `P…` and the SQL-standard combined forms are deferred.
//!
//! This is a §8 determinism hotspot: the fractional-unit cascade, the µs rounding (half away
//! from zero — the engine's one mode), and the render format must be byte-identical across the
//! Rust/Go/TS cores. All cascade arithmetic is exact integer math (no float in the value path).

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

/// An ASCII-whitespace byte — the fixed set separating/trimming interval tokens (not
/// locale/Unicode-dependent, identical across cores).
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r')
}

/// An accumulator building the three fields exactly. The fractional part of each unit token
/// cascades down (months→days→micros) using exact integer math (a numerator/denominator), with
/// the µs result rounded half away from zero. Field overflow traps 22008.
struct Acc {
    months: i64,
    days: i64,
    micros: i64,
}

impl Acc {
    fn new() -> Acc {
        Acc {
            months: 0,
            days: 0,
            micros: 0,
        }
    }

    fn add_months(&mut self, m: i64) -> Result<()> {
        self.months = self
            .months
            .checked_add(m)
            .ok_or_else(|| field_overflow("interval out of range"))?;
        Ok(())
    }
    fn add_days(&mut self, d: i64) -> Result<()> {
        self.days = self
            .days
            .checked_add(d)
            .ok_or_else(|| field_overflow("interval out of range"))?;
        Ok(())
    }
    fn add_micros(&mut self, u: i64) -> Result<()> {
        self.micros = self
            .micros
            .checked_add(u)
            .ok_or_else(|| field_overflow("interval out of range"))?;
        Ok(())
    }
}

/// Round `num / den` to the nearest integer, half away from zero (the engine's one rounding mode
/// — spec/design/decimal.md §3). `den > 0`. Used for the bottom-of-cascade µs result.
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

/// Add `value = sign * whole.frac` of a unit to the accumulator, where the unit is measured in
/// `months_per`, `days_per`, or `micros_per` of one base field (exactly one is nonzero). The
/// integer part lands in that field; the fractional part cascades to the next-lower fields using
/// 1 month = 30 days and 1 day = 24 h (spec/design/interval.md §3). All exact integer math.
fn apply_unit(
    acc: &mut Acc,
    neg: bool,
    int_part: i128,
    frac_num: i128,
    frac_den: i128, // a power of ten > 0; the fraction is frac_num/frac_den, 0 <= frac_num < frac_den
    // The unit's cascade weights (months_per, days_per, micros_per) — exactly one is nonzero.
    per: (i128, i128, i128),
) -> Result<()> {
    let (months_per, days_per, micros_per) = per;
    // The exact value of the token as a fraction N/D (signed).
    let sign = if neg { -1i128 } else { 1 };
    let n = sign * (int_part * frac_den + frac_num);
    let d = frac_den;

    let mut months = 0i128;
    let mut days = 0i128;
    // Exact micros numerator (over d) accumulated from every cascade level, rounded once at the end.
    let mut micros_num = 0i128;

    if months_per != 0 {
        let total = n * months_per; // months * d
        months = total / d; // trunc toward zero
        let rem = total - months * d; // remaining fractional months * d
        let day_total = rem * DAYS_PER_MONTH as i128; // days * d
        let whole_days = day_total / d;
        days += whole_days;
        let rem_days = day_total - whole_days * d;
        micros_num += rem_days * MICROS_PER_DAY as i128;
    }
    if days_per != 0 {
        let total = n * days_per; // days * d
        let whole_days = total / d;
        days += whole_days;
        let rem_days = total - whole_days * d;
        micros_num += rem_days * MICROS_PER_DAY as i128;
    }
    if micros_per != 0 {
        micros_num += n * micros_per; // micros * d
    }

    if months != 0 {
        acc.add_months(
            i64::try_from(months).map_err(|_| field_overflow("interval out of range"))?,
        )?;
    }
    if days != 0 {
        acc.add_days(i64::try_from(days).map_err(|_| field_overflow("interval out of range"))?)?;
    }
    if micros_num != 0 {
        let u = round_div(micros_num, d);
        acc.add_micros(i64::try_from(u).map_err(|_| field_overflow("interval out of range"))?)?;
    }
    Ok(())
}

/// The cascade weights (`months_per`, `days_per`, `micros_per`) for a unit word (case-insensitive),
/// or None for an unrecognized unit. Exactly one weight is nonzero. The "unit + time" subset
/// (spec/design/interval.md §3); ambiguous bare `m` is intentionally not accepted.
fn unit_weights(unit: &str) -> Option<(i128, i128, i128)> {
    let u = unit.to_ascii_lowercase();
    Some(match u.as_str() {
        "millennium" | "millennia" | "mil" | "mils" => (12000 * MONTHS_PER_YEAR as i128 / 12, 0, 0),
        "century" | "centuries" | "cent" | "c" => (1200, 0, 0),
        "decade" | "decades" | "dec" | "decs" => (120, 0, 0),
        "year" | "years" | "yr" | "yrs" | "y" => (MONTHS_PER_YEAR as i128, 0, 0),
        "month" | "months" | "mon" | "mons" => (1, 0, 0),
        "week" | "weeks" | "w" => (0, 7, 0),
        "day" | "days" | "d" => (0, 1, 0),
        "hour" | "hours" | "hr" | "hrs" | "h" => (0, 0, 3600 * MICROS_PER_SEC as i128),
        "minute" | "minutes" | "min" | "mins" => (0, 0, 60 * MICROS_PER_SEC as i128),
        "second" | "seconds" | "sec" | "secs" | "s" => (0, 0, MICROS_PER_SEC as i128),
        "millisecond" | "milliseconds" | "msec" | "msecs" | "ms" => (0, 0, 1000),
        "microsecond" | "microseconds" | "usec" | "usecs" | "us" => (0, 0, 1),
        _ => return None,
    })
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && is_ws(self.b[self.i]) {
            self.i += 1;
        }
    }
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
}

/// The maximum integer digits in one numeric token, and the maximum fractional digits — bounds
/// chosen so the exact i128 cascade math cannot overflow (and Go big.Int / TS BigInt agree). An
/// over-long run traps 22008. Far beyond any real interval.
const MAX_INT_DIGITS: usize = 18;
const MAX_FRAC_DIGITS: usize = 9;

/// Read a run of ASCII digits as an i128. Empty run → Ok(None) (caller raises 22007); more than
/// `MAX_INT_DIGITS` digits → Err(22008).
fn read_digits(c: &mut Cursor) -> Result<Option<i128>> {
    let start = c.i;
    let mut v: i128 = 0;
    while c.i < c.b.len() && c.b[c.i].is_ascii_digit() {
        v = v * 10 + (c.b[c.i] - b'0') as i128;
        c.i += 1;
    }
    if c.i == start {
        return Ok(None);
    }
    if c.i - start > MAX_INT_DIGITS {
        return Err(field_overflow("interval field has too many digits"));
    }
    Ok(Some(v))
}

/// Parse the time fields after an integer hour and a `:` — `MM[:SS[.ffffff]]` — adding their
/// micros to `acc` with the given sign. `hour` is the already-read integer hour (unbounded; PG
/// allows `100:00:00`). Sub-µs digits round half away from zero (like timestamp).
fn parse_time(acc: &mut Acc, c: &mut Cursor, neg: bool, hour: i128) -> Result<()> {
    // ':' already at cursor.
    c.i += 1; // consume ':'
    let minute = read_digits(c)?.ok_or_else(|| invalid_format("expected minutes"))?;
    let mut second: i128 = 0;
    let mut frac_us: i128 = 0;
    if c.peek() == Some(b':') {
        c.i += 1;
        second = read_digits(c)?.ok_or_else(|| invalid_format("expected seconds"))?;
        if c.peek() == Some(b'.') {
            c.i += 1;
            frac_us = read_frac_us(c)?;
        }
    }
    // Compose the time as exact µs and add with the sign.
    let total_us = hour
        .checked_mul(3600 * MICROS_PER_SEC as i128)
        .and_then(|h| {
            minute
                .checked_mul(60 * MICROS_PER_SEC as i128)
                .map(|m| h + m)
        })
        .and_then(|hm| second.checked_mul(MICROS_PER_SEC as i128).map(|s| hm + s))
        .map(|hms| hms + frac_us)
        .ok_or_else(|| field_overflow("interval out of range"))?;
    let signed = if neg { -total_us } else { total_us };
    acc.add_micros(i64::try_from(signed).map_err(|_| field_overflow("interval out of range"))?)
}

/// Read the fractional-seconds digits after `.` into microseconds (0..=1_000_000), 0–6 digits
/// exact, 7+ rounded half away from zero — identical to the timestamp module's rule.
fn read_frac_us(c: &mut Cursor) -> Result<i128> {
    let start = c.i;
    while c.i < c.b.len() && c.b[c.i].is_ascii_digit() {
        c.i += 1;
    }
    let digits = &c.b[start..c.i];
    if digits.is_empty() {
        return Err(invalid_format("expected fractional digits after '.'"));
    }
    let mut us: i128 = 0;
    for k in 0..6 {
        us *= 10;
        if k < digits.len() {
            us += (digits[k] - b'0') as i128;
        }
    }
    if digits.len() > 6 && digits[6] >= b'5' {
        us += 1;
    }
    Ok(us)
}

/// Parse an interval literal (the "unit + time" subset) into the three-field value. Errors:
/// malformed syntax → 22007; a field beyond the representable range → 22008. Parsing happens at
/// resolve time, before any scan, so a bad literal traps deterministically (like timestamp/bytea).
pub fn parse_interval(input: &str) -> Result<Interval> {
    let trimmed = input.trim_matches(|ch: char| ch.is_ascii_whitespace());
    let b = trimmed.as_bytes();
    let mut c = Cursor { b, i: 0 };
    let mut acc = Acc::new();

    c.skip_ws();
    // An optional leading `@` (PG's verbose lead-in) is accepted and ignored.
    if c.peek() == Some(b'@') {
        c.i += 1;
        c.skip_ws();
    }
    if c.done() {
        return Err(invalid_format("empty interval"));
    }

    let mut ago = false;
    let mut saw_field = false;
    while !c.done() {
        // Trailing `ago` negates the whole interval; nothing may follow it.
        if let Some(word) = peek_word(&c)
            && word.eq_ignore_ascii_case("ago")
        {
            c.i += word.len();
            ago = true;
            c.skip_ws();
            break;
        }

        // Optional per-segment sign.
        let mut neg = false;
        match c.peek() {
            Some(b'-') => {
                neg = true;
                c.i += 1;
            }
            Some(b'+') => {
                c.i += 1;
            }
            _ => {}
        }
        let int_part = read_digits(&mut c)?.ok_or_else(|| invalid_format("expected a number"))?;

        if c.peek() == Some(b':') {
            // A bare time: this integer is the hour. (No fractional hour before `:`.)
            parse_time(&mut acc, &mut c, neg, int_part)?;
            saw_field = true;
        } else {
            // An optional fractional part on a unit (or bare-seconds) value.
            let (frac_num, frac_den) = if c.peek() == Some(b'.') {
                c.i += 1;
                read_unit_frac(&mut c)?
            } else {
                (0, 1)
            };
            c.skip_ws();
            // A bare number with no unit defaults to SECONDS (PG `interval '5'` = 5 seconds);
            // a trailing `ago` is left for the loop top. A recognized unit applies its weights;
            // an unrecognized unit word is a 22007.
            let secs = (0, 0, MICROS_PER_SEC as i128);
            let per = match peek_word(&c) {
                Some(u) if u.eq_ignore_ascii_case("ago") => secs,
                Some(u) => {
                    c.i += u.len();
                    unit_weights(&u)
                        .ok_or_else(|| invalid_format(format!("unknown interval unit \"{u}\"")))?
                }
                None => secs,
            };
            apply_unit(&mut acc, neg, int_part, frac_num, frac_den, per)?;
            saw_field = true;
        }
        c.skip_ws();
    }

    if !c.done() {
        return Err(invalid_format("trailing characters in interval"));
    }
    if !saw_field {
        return Err(invalid_format("empty interval"));
    }

    if ago {
        acc.months = acc
            .months
            .checked_neg()
            .ok_or_else(|| field_overflow("interval out of range"))?;
        acc.days = acc
            .days
            .checked_neg()
            .ok_or_else(|| field_overflow("interval out of range"))?;
        acc.micros = acc
            .micros
            .checked_neg()
            .ok_or_else(|| field_overflow("interval out of range"))?;
    }

    Ok(Interval {
        months: i32::try_from(acc.months).map_err(|_| field_overflow("interval out of range"))?,
        days: i32::try_from(acc.days).map_err(|_| field_overflow("interval out of range"))?,
        micros: acc.micros,
    })
}

/// Read a unit value's fractional digits after `.` as `(numerator, denominator)` with the
/// denominator a power of ten. Caps at 18 digits (more than enough; an overlong run traps 22007).
fn read_unit_frac(c: &mut Cursor) -> Result<(i128, i128)> {
    let start = c.i;
    while c.i < c.b.len() && c.b[c.i].is_ascii_digit() {
        c.i += 1;
    }
    let digits = &c.b[start..c.i];
    if digits.is_empty() {
        return Err(invalid_format("expected fractional digits after '.'"));
    }
    if digits.len() > MAX_FRAC_DIGITS {
        return Err(field_overflow(
            "interval value has too many fractional digits",
        ));
    }
    let mut num: i128 = 0;
    let mut den: i128 = 1;
    for &d in digits {
        num = num * 10 + (d - b'0') as i128;
        den *= 10;
    }
    Ok((num, den))
}

/// Peek an ASCII-letter word at the cursor (not consuming). Interval units and `ago` are
/// letters-only; a non-letter (digit, sign, `:`) ends the word.
fn peek_word(c: &Cursor) -> Option<String> {
    let start = c.i;
    let mut j = start;
    while j < c.b.len() && c.b[j].is_ascii_alphabetic() {
        j += 1;
    }
    if j == start {
        None
    } else {
        Some(String::from_utf8_lossy(&c.b[start..j]).into_owned())
    }
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
