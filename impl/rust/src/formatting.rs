//! The data-type formatting functions — to_char, to_timestamp, to_date, to_number
//! (spec/design/formatting.md). A transcription of PostgreSQL 18's formatting.c: the template parser
//! (parse_format), the datetime renderer (DCH_to_char) and reader (DCH_from_char + do_to_timestamp).
//! The number half (NUM_processor and the per-type to_char preparers, to_number) is
//! `formatting_num.rs`. The template vocabulary is shared spec data (`formatting_gen.rs` from
//! spec/types/formatting.toml); everything here is the hand-written logic that consumes it. jed has
//! no locale, so every locale-dependent piece behaves as PostgreSQL does under the C locale: TM
//! renders the English names, and D / G / L / S / MI / PL are ".", ",", " ", "-", "+". The
//! processors work on UTF-8 BYTES, exactly as formatting.c does, so the three cores agree byte for
//! byte; C int arithmetic is reproduced with `i32` (PostgreSQL builds with -fwrapv, so its int
//! overflow wraps — hence the explicit `wrapping_*` operations) wherever an operand can leave the
//! int range.

// The generated pattern ids keep PostgreSQL's case-distinguished names (DCH_am, NUM_rn, …).
#![allow(non_upper_case_globals)]

use crate::cost::Meter;
use crate::costs::COSTS;
use crate::error::{EngineError, Result, SqlState};
use crate::formatting_gen::*;
use crate::formatting_num::NumDesc;
use crate::interval::Interval;
use crate::timezone::ZoneRef;

// --- template nodes --------------------------------------------------------------------------

// The format-node kinds (formatting.c NODE_TYPE_*).
pub(crate) const FN_END: u8 = 0;
pub(crate) const FN_ACTION: u8 = 1;
pub(crate) const FN_CHAR: u8 = 2;
pub(crate) const FN_SEPARATOR: u8 = 3;
pub(crate) const FN_SPACE: u8 = 4;

/// One parsed template element: a pattern (`key`) with its suffix bits, or one literal character
/// (`chars`, a whole UTF-8 sequence).
#[derive(Clone, Copy)]
pub(crate) struct FmtNode<'a> {
    pub(crate) typ: u8,
    pub(crate) chars: &'a [u8],
    pub(crate) suffix: u8,
    pub(crate) key: Option<&'static FmtKeyword>,
}

impl FmtNode<'_> {
    /// The pattern of an action node.
    pub(crate) fn key(&self) -> &'static FmtKeyword {
        self.key.expect("an action node carries its keyword")
    }
}

/// C's isspace in the C locale.
pub(crate) fn is_space_c(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

pub(crate) fn is_digit_c(b: u8) -> bool {
    b.is_ascii_digit()
}

fn is_alpha_c(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// formatting.c's is_separator_char: printable ASCII that is not a letter or digit.
fn is_separator_char(b: u8) -> bool {
    b > 0x20 && b < 0x7f && !is_alpha_c(b) && !is_digit_c(b)
}

/// PostgreSQL's pg_utf_mblen of the character at `s[i]`, clipped to the string.
pub(crate) fn pg_mblen(s: &[u8], i: usize) -> usize {
    let c = s[i];
    let mut n = 1;
    if c & 0x80 == 0 {
        n = 1;
    } else if c & 0xe0 == 0xc0 {
        n = 2;
    } else if c & 0xf0 == 0xe0 {
        n = 3;
    } else if c & 0xf8 == 0xf0 {
        n = 4;
    }
    if i + n > s.len() {
        n = s.len() - i;
    }
    n
}

/// formatting.c's index_seq_search: the first keyword, in table order from the first one sharing
/// `s`'s first byte, whose name is a prefix of `s`.
fn fmt_index_search(
    s: &[u8],
    kws: &'static [FmtKeyword],
    index: &[i16; 128],
) -> Option<&'static FmtKeyword> {
    let c = s[0];
    if c <= b' ' || c >= b'~' {
        return None;
    }
    let k = index[c as usize];
    if k < 0 {
        return None;
    }
    let mut k = k as usize;
    loop {
        if s.starts_with(kws[k].name.as_bytes()) {
            return Some(&kws[k]);
        }
        k += 1;
        if k == kws.len() || kws[k].name.as_bytes()[0] != c {
            return None;
        }
    }
}

/// formatting.c's suff_search over DCH_suff for one suffix type.
fn fmt_suffix_search(s: &[u8], prefix: bool) -> Option<&'static FmtSuffix> {
    DCH_SUFFIXES
        .iter()
        .find(|sf| sf.prefix == prefix && s.starts_with(sf.name.as_bytes()))
}

/// formatting.c's parse_format: the template as a node list ending in an `FN_END` node. A number
/// template (`dch = false`) also fills `num`, which can reject the template (42601).
pub(crate) fn parse_format<'a>(
    s: &'a [u8],
    dch: bool,
    num: &mut NumDesc,
) -> Result<Vec<FmtNode<'a>>> {
    let (kws, index) = if dch {
        (DCH_KEYWORDS, &DCH_INDEX)
    } else {
        (NUM_KEYWORDS, &NUM_INDEX)
    };
    let mut nodes: Vec<FmtNode<'a>> = Vec::with_capacity(s.len() + 1);
    let mut i = 0;
    while i < s.len() {
        let mut suffix = 0u8;
        if dch && let Some(sf) = fmt_suffix_search(&s[i..], true) {
            suffix |= sf.flag;
            i += sf.name.len();
        }
        if i >= s.len() {
            break;
        }
        if let Some(k) = fmt_index_search(&s[i..], kws, index) {
            let mut n = FmtNode {
                typ: FN_ACTION,
                chars: &[],
                suffix,
                key: Some(k),
            };
            i += k.name.len();
            if !dch {
                num.prepare(k.id)?;
            }
            if dch
                && i < s.len()
                && let Some(sf) = fmt_suffix_search(&s[i..], false)
            {
                n.suffix |= sf.flag;
                i += sf.name.len();
            }
            nodes.push(n);
            continue;
        }
        if s[i] == b'"' {
            // A double-quoted literal; a backslash quotes the next character.
            i += 1;
            while i < s.len() {
                if s[i] == b'"' {
                    i += 1;
                    break;
                }
                if s[i] == b'\\' && i + 1 < s.len() {
                    i += 1;
                }
                let cl = pg_mblen(s, i);
                nodes.push(FmtNode {
                    typ: FN_CHAR,
                    chars: &s[i..i + cl],
                    suffix: 0,
                    key: None,
                });
                i += cl;
            }
            continue;
        }
        // Outside quotes a backslash is special only before a double quote.
        if s[i] == b'\\' && i + 1 < s.len() && s[i + 1] == b'"' {
            i += 1;
        }
        let cl = pg_mblen(s, i);
        let typ = if dch && is_separator_char(s[i]) {
            FN_SEPARATOR
        } else if is_space_c(s[i]) {
            FN_SPACE
        } else {
            FN_CHAR
        };
        nodes.push(FmtNode {
            typ,
            chars: &s[i..i + cl],
            suffix: 0,
            key: None,
        });
        i += cl;
    }
    nodes.push(FmtNode {
        typ: FN_END,
        chars: &[],
        suffix: 0,
        key: None,
    });
    Ok(nodes)
}

// --- C formatting helpers --------------------------------------------------------------------

/// C's "%0*d": `v` zero-padded to width `w` (the sign counts toward the width; 0 = none).
pub(crate) fn c_pad_int(v: i64, w: usize) -> String {
    let s = v.to_string();
    if s.len() >= w {
        return s;
    }
    if v < 0 {
        return format!("-{}{}", "0".repeat(w - s.len()), &s[1..]);
    }
    format!("{}{}", "0".repeat(w - s.len()), s)
}

/// C's "%-*s": `s` space-padded on the right to `w` bytes.
fn c_pad_right(s: &str, w: usize) -> String {
    if s.len() >= w {
        return s.to_string();
    }
    format!("{}{}", s, " ".repeat(w - s.len()))
}

/// C's "%*s": `s` space-padded on the left to `w` bytes.
pub(crate) fn c_pad_left(s: &str, w: usize) -> String {
    if s.len() >= w {
        return s.to_string();
    }
    format!("{}{}", " ".repeat(w - s.len()), s)
}

/// `s` double-quoted with `"` and `\` escaped (the spelling the other cores give a rendered
/// number in the ordinal-suffix error).
fn quote_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// formatting.c's get_th: the ordinal suffix of a rendered number (22P02 when its last byte is not
/// a digit).
pub(crate) fn fmt_ordinal(num: &[u8], upper: bool) -> Result<&'static str> {
    let mut last = num.last().copied().unwrap_or(0);
    if !is_digit_c(last) {
        return Err(EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!(
                "{} is not a number",
                quote_str(&String::from_utf8_lossy(num))
            ),
        ));
    }
    if num.len() > 1 && num[num.len() - 2] == b'1' {
        last = 0;
    }
    let idx = match last {
        b'1' => 0,
        b'2' => 1,
        b'3' => 2,
        _ => 3,
    };
    Ok(if upper {
        WORDS_ORDINAL_UPPER[idx]
    } else {
        WORDS_ORDINAL_LOWER[idx]
    })
}

/// PostgreSQL's asc_toupper / asc_tolower over a string.
fn ascii_upper(s: &str) -> String {
    s.to_ascii_uppercase()
}

pub(crate) fn ascii_lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// The bytes as text: they are whole UTF-8 sequences in practice; anything else is replaced.
pub(crate) fn bytes_to_string(b: Vec<u8>) -> String {
    match String::from_utf8(b) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// --- PostgreSQL's calendar helpers (datetime.c / timestamp.c), C int arithmetic --------------

/// datetime.c's date2j: the Julian day of a (proleptic Gregorian) date, in wrapping int32
/// arithmetic.
fn pg_date2j(year: i32, month: i32, day: i32) -> i32 {
    let (mut year, mut month) = (year, month);
    if month > 2 {
        month = month.wrapping_add(1);
        year = year.wrapping_add(4800);
    } else {
        month = month.wrapping_add(13);
        year = year.wrapping_add(4799);
    }
    let century = year / 100;
    let mut julian = year.wrapping_mul(365).wrapping_sub(32167);
    julian = julian
        .wrapping_add(year / 4)
        .wrapping_sub(century)
        .wrapping_add(century / 4);
    julian = julian
        .wrapping_add(7834i32.wrapping_mul(month) / 256)
        .wrapping_add(day);
    julian
}

/// datetime.c's j2date (unsigned int arithmetic).
fn pg_j2date(jd: i32) -> (i32, i32, i32) {
    let mut julian = jd as u32;
    julian = julian.wrapping_add(32044);
    let mut quad = julian / 146097;
    let extra = julian
        .wrapping_sub(quad.wrapping_mul(146097))
        .wrapping_mul(4)
        .wrapping_add(3);
    julian = julian
        .wrapping_add(60)
        .wrapping_add(quad.wrapping_mul(3))
        .wrapping_add(extra / 146097);
    quad = julian / 1461;
    julian = julian.wrapping_sub(quad.wrapping_mul(1461));
    let mut y = (julian.wrapping_mul(4) / 1461) as i32;
    if y != 0 {
        julian = julian.wrapping_add(305) % 365 + 123;
    } else {
        julian = julian.wrapping_add(306) % 366 + 123;
    }
    y = y.wrapping_add(quad.wrapping_mul(4) as i32);
    let year = y.wrapping_sub(4800);
    quad = julian.wrapping_mul(2141) / 65536;
    let day = julian.wrapping_sub(7834u32.wrapping_mul(quad) / 256) as i32;
    let month = (quad.wrapping_add(10) % 12 + 1) as i32;
    (year, month, day)
}

/// datetime.c's j2day: 0..6 = Sunday..Saturday.
fn pg_j2day(date: i32) -> i32 {
    let mut d = date.wrapping_add(1);
    d %= 7;
    if d < 0 {
        d += 7;
    }
    d
}

/// timestamp.c's isoweek2j.
fn pg_isoweek2j(year: i32, week: i32) -> i32 {
    let day4 = pg_date2j(year, 1, 4);
    let day0 = pg_j2day(day4.wrapping_sub(1));
    week.wrapping_sub(1)
        .wrapping_mul(7)
        .wrapping_add(day4.wrapping_sub(day0))
}

/// timestamp.c's date2isoweek.
fn pg_date2isoweek(year: i32, mon: i32, mday: i32) -> i32 {
    let dayn = pg_date2j(year, mon, mday);
    let mut day4 = pg_date2j(year, 1, 4);
    let mut day0 = pg_j2day(day4.wrapping_sub(1));
    if dayn < day4.wrapping_sub(day0) {
        day4 = pg_date2j(year.wrapping_sub(1), 1, 4);
        day0 = pg_j2day(day4.wrapping_sub(1));
    }
    let mut result = dayn.wrapping_sub(day4.wrapping_sub(day0)) / 7 + 1;
    if result >= 52 {
        day4 = pg_date2j(year.wrapping_add(1), 1, 4);
        day0 = pg_j2day(day4.wrapping_sub(1));
        if dayn >= day4.wrapping_sub(day0) {
            result = dayn.wrapping_sub(day4.wrapping_sub(day0)) / 7 + 1;
        }
    }
    result
}

/// timestamp.c's date2isoyear.
fn pg_date2isoyear(year: i32, mon: i32, mday: i32) -> i32 {
    let mut year = year;
    let dayn = pg_date2j(year, mon, mday);
    let mut day4 = pg_date2j(year, 1, 4);
    let mut day0 = pg_j2day(day4.wrapping_sub(1));
    if dayn < day4.wrapping_sub(day0) {
        day4 = pg_date2j(year.wrapping_sub(1), 1, 4);
        day0 = pg_j2day(day4.wrapping_sub(1));
        year = year.wrapping_sub(1);
    }
    let result = dayn.wrapping_sub(day4.wrapping_sub(day0)) / 7 + 1;
    if result >= 52 {
        day4 = pg_date2j(year.wrapping_add(1), 1, 4);
        day0 = pg_j2day(day4.wrapping_sub(1));
        if dayn >= day4.wrapping_sub(day0) {
            year = year.wrapping_add(1);
        }
    }
    year
}

/// timestamp.c's date2isoyearday.
fn pg_date2isoyearday(year: i32, mon: i32, mday: i32) -> i32 {
    pg_date2j(year, mon, mday)
        .wrapping_sub(pg_isoweek2j(pg_date2isoyear(year, mon, mday), 1))
        .wrapping_add(1)
}

/// PostgreSQL's isleap.
fn pg_is_leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

const PG_DAY_TAB: [[i32; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];

// --- to_char for datetimes and intervals (DCH_to_char) ---------------------------------------

/// formatting.c's fmt_tm: a broken-down datetime or interval. `hour` is 64-bit (an interval's hours
/// are unbounded); the rest are C ints.
#[derive(Clone, Copy, Default)]
struct FmtTm {
    sec: i32,
    min: i32,
    hour: i64,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    gmtoff: i64,
}

fn invalid_for_interval() -> EngineError {
    EngineError::new(
        SqlState::InvalidDatetimeFormat,
        "invalid format specification for an interval value",
    )
}

/// formatting.c's ADJUST_YEAR: a BC year shown positive (an interval's as is).
fn adjust_year(year: i32, is_interval: bool) -> i32 {
    if is_interval {
        return year;
    }
    if year <= 0 {
        return year.wrapping_sub(1).wrapping_neg();
    }
    year
}

fn has_fm(n: &FmtNode) -> bool {
    n.suffix & SUFFIX_FM != 0
}

fn has_tm(n: &FmtNode) -> bool {
    n.suffix & SUFFIX_TM != 0
}

/// A "%0*d" width: 0 in fill mode, else `w` (`w + 1` for a negative value).
fn dch_width(n: &FmtNode, w: usize, neg: bool) -> usize {
    if has_fm(n) {
        0
    } else if neg {
        w + 1
    } else {
        w
    }
}

/// Write a number with its optional ordinal suffix (TH / th).
fn put_num(b: &mut Vec<u8>, n: &FmtNode, s: String) -> Result<()> {
    let mut s = s;
    if n.suffix & (SUFFIX_TH | SUFFIX_th) != 0 {
        let th = fmt_ordinal(s.as_bytes(), n.suffix & SUFFIX_TH != 0)?;
        s.push_str(th);
    }
    b.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Write a padded (or, with TM / FM, bare) month or day name in the requested case.
fn put_word(b: &mut Vec<u8>, n: &FmtNode, name: &str, case_mode: u8, pad: bool) {
    let mut name = match case_mode {
        1 => ascii_upper(name),
        2 => ascii_lower(name),
        _ => name.to_string(),
    };
    if pad && !has_tm(n) && !has_fm(n) {
        name = c_pad_right(&name, 9);
    }
    b.extend_from_slice(name.as_bytes());
}

/// A name pattern's case: 1 upper, 2 lower, 0 the name as written (initcap).
fn case_of(id: u8, upper: u8, lower: u8) -> u8 {
    if id == upper {
        1
    } else if id == lower {
        2
    } else {
        0
    }
}

fn offset_sign(gmtoff: i64) -> &'static str {
    if gmtoff >= 0 { "+" } else { "-" }
}

/// formatting.c's DCH_to_char.
fn dch_to_char(
    nodes: &[FmtNode],
    is_interval: bool,
    tm: &FmtTm,
    fsec: i64,
    tzn: Option<&str>,
) -> Result<String> {
    let mut b: Vec<u8> = Vec::new();
    let pm = tm.hour % 24 >= 12;
    for n in nodes {
        if n.typ == FN_END {
            break;
        }
        if n.typ != FN_ACTION {
            b.extend_from_slice(n.chars);
            continue;
        }
        let key = n.key();
        let id = key.id;
        match id {
            DCH_A_M | DCH_P_M => b.extend_from_slice(if pm { b"P.M." } else { b"A.M." }),
            DCH_AM | DCH_PM => b.extend_from_slice(if pm { b"PM" } else { b"AM" }),
            DCH_a_m | DCH_p_m => b.extend_from_slice(if pm { b"p.m." } else { b"a.m." }),
            DCH_am | DCH_pm => b.extend_from_slice(if pm { b"pm" } else { b"am" }),
            DCH_HH | DCH_HH12 => {
                let mut v = tm.hour % 12;
                if v == 0 {
                    v = 12;
                }
                put_num(&mut b, n, c_pad_int(v, dch_width(n, 2, tm.hour < 0)))?;
            }
            DCH_HH24 => put_num(&mut b, n, c_pad_int(tm.hour, dch_width(n, 2, tm.hour < 0)))?,
            DCH_MI => put_num(
                &mut b,
                n,
                c_pad_int(tm.min as i64, dch_width(n, 2, tm.min < 0)),
            )?,
            DCH_SS => put_num(
                &mut b,
                n,
                c_pad_int(tm.sec as i64, dch_width(n, 2, tm.sec < 0)),
            )?,
            DCH_FF1 => put_num(&mut b, n, c_pad_int(fsec / 100000, 1))?,
            DCH_FF2 => put_num(&mut b, n, c_pad_int(fsec / 10000, 2))?,
            DCH_FF3 | DCH_MS => put_num(&mut b, n, c_pad_int(fsec / 1000, 3))?,
            DCH_FF4 => put_num(&mut b, n, c_pad_int(fsec / 100, 4))?,
            DCH_FF5 => put_num(&mut b, n, c_pad_int(fsec / 10, 5))?,
            DCH_FF6 | DCH_US => put_num(&mut b, n, c_pad_int(fsec, 6))?,
            DCH_SSSS => put_num(
                &mut b,
                n,
                (tm.hour * 3600 + tm.min as i64 * 60 + tm.sec as i64).to_string(),
            )?,
            DCH_tz | DCH_TZ => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                if let Some(p) = tzn {
                    let p = if id == DCH_tz {
                        ascii_lower(p)
                    } else {
                        p.to_string()
                    };
                    if p.len() > key.name.len() * 12 {
                        return Err(EngineError::new(
                            SqlState::DatetimeFieldOverflow,
                            "time zone format value too long",
                        ));
                    }
                    b.extend_from_slice(p.as_bytes());
                }
            }
            DCH_TZH => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                b.extend_from_slice(offset_sign(tm.gmtoff).as_bytes());
                b.extend_from_slice(c_pad_int(tm.gmtoff.abs() / 3600, 2).as_bytes());
            }
            DCH_TZM => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                b.extend_from_slice(c_pad_int((tm.gmtoff.abs() % 3600) / 60, 2).as_bytes());
            }
            DCH_OF => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                let w = if has_fm(n) { 0 } else { 2 };
                b.extend_from_slice(offset_sign(tm.gmtoff).as_bytes());
                b.extend_from_slice(c_pad_int(tm.gmtoff.abs() / 3600, w).as_bytes());
                if tm.gmtoff.abs() % 3600 != 0 {
                    b.push(b':');
                    b.extend_from_slice(c_pad_int((tm.gmtoff.abs() % 3600) / 60, 2).as_bytes());
                }
            }
            DCH_A_D | DCH_B_C | DCH_AD | DCH_BC | DCH_a_d | DCH_b_c | DCH_ad | DCH_bc => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                let bc = tm.year <= 0;
                let s: &[u8] = match id {
                    DCH_A_D | DCH_B_C => {
                        if bc {
                            b"B.C."
                        } else {
                            b"A.D."
                        }
                    }
                    DCH_AD | DCH_BC => {
                        if bc {
                            b"BC"
                        } else {
                            b"AD"
                        }
                    }
                    DCH_a_d | DCH_b_c => {
                        if bc {
                            b"b.c."
                        } else {
                            b"a.d."
                        }
                    }
                    _ => {
                        if bc {
                            b"bc"
                        } else {
                            b"ad"
                        }
                    }
                };
                b.extend_from_slice(s);
            }
            DCH_MONTH | DCH_Month | DCH_month => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                if tm.mon != 0 {
                    put_word(
                        &mut b,
                        n,
                        WORDS_MONTHS_FULL[(tm.mon - 1) as usize],
                        case_of(id, DCH_MONTH, DCH_month),
                        true,
                    );
                }
            }
            DCH_MON | DCH_Mon | DCH_mon => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                if tm.mon != 0 {
                    put_word(
                        &mut b,
                        n,
                        WORDS_MONTHS_ABBREV[(tm.mon - 1) as usize],
                        case_of(id, DCH_MON, DCH_mon),
                        false,
                    );
                }
            }
            DCH_MM => put_num(
                &mut b,
                n,
                c_pad_int(tm.mon as i64, dch_width(n, 2, tm.mon < 0)),
            )?,
            DCH_DAY | DCH_Day | DCH_day => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                put_word(
                    &mut b,
                    n,
                    WORDS_DAYS_FULL[tm.wday as usize],
                    case_of(id, DCH_DAY, DCH_day),
                    true,
                );
            }
            DCH_DY | DCH_Dy | DCH_dy => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                put_word(
                    &mut b,
                    n,
                    WORDS_DAYS_ABBREV[tm.wday as usize],
                    case_of(id, DCH_DY, DCH_dy),
                    false,
                );
            }
            DCH_DDD | DCH_IDDD => {
                let v = if id == DCH_IDDD {
                    pg_date2isoyearday(tm.year, tm.mon, tm.mday)
                } else {
                    tm.yday
                };
                put_num(&mut b, n, c_pad_int(v as i64, dch_width(n, 3, false)))?;
            }
            DCH_DD => put_num(&mut b, n, c_pad_int(tm.mday as i64, dch_width(n, 2, false)))?,
            DCH_D => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                put_num(&mut b, n, (tm.wday as i64 + 1).to_string())?;
            }
            DCH_ID => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
                let v = if tm.wday == 0 { 7 } else { tm.wday };
                put_num(&mut b, n, v.to_string())?;
            }
            DCH_WW => put_num(
                &mut b,
                n,
                c_pad_int(
                    (tm.yday.wrapping_sub(1) / 7).wrapping_add(1) as i64,
                    dch_width(n, 2, false),
                ),
            )?,
            DCH_IW => put_num(
                &mut b,
                n,
                c_pad_int(
                    pg_date2isoweek(tm.year, tm.mon, tm.mday) as i64,
                    dch_width(n, 2, false),
                ),
            )?,
            DCH_Q => {
                if tm.mon != 0 {
                    put_num(&mut b, n, ((tm.mon - 1) / 3 + 1).to_string())?;
                }
            }
            DCH_CC => {
                let c: i32 = if is_interval {
                    tm.year / 100
                } else if tm.year > 0 {
                    (tm.year.wrapping_sub(1) / 100).wrapping_add(1)
                } else {
                    (tm.year / 100).wrapping_sub(1)
                };
                if (-99..=99).contains(&c) {
                    put_num(&mut b, n, c_pad_int(c as i64, dch_width(n, 2, c < 0)))?;
                } else {
                    put_num(&mut b, n, c.to_string())?;
                }
            }
            DCH_Y_YYY => {
                let y = adjust_year(tm.year, is_interval);
                let k = y / 1000;
                put_num(
                    &mut b,
                    n,
                    format!(
                        "{},{}",
                        k,
                        c_pad_int(y.wrapping_sub(k.wrapping_mul(1000)) as i64, 3)
                    ),
                )?;
            }
            DCH_YYYY | DCH_IYYY | DCH_YYY | DCH_IYY | DCH_YY | DCH_IY | DCH_Y | DCH_I => {
                let adj = adjust_year(tm.year, is_interval);
                let mut y = adj;
                if matches!(id, DCH_IYYY | DCH_IYY | DCH_IY | DCH_I) {
                    y = adjust_year(pg_date2isoyear(tm.year, tm.mon, tm.mday), is_interval);
                }
                let w = match id {
                    DCH_YYYY | DCH_IYYY => dch_width(n, 4, adj < 0),
                    DCH_YYY | DCH_IYY => {
                        y %= 1000;
                        dch_width(n, 3, adj < 0)
                    }
                    DCH_YY | DCH_IY => {
                        y %= 100;
                        dch_width(n, 2, adj < 0)
                    }
                    _ => {
                        y %= 10;
                        1
                    }
                };
                put_num(&mut b, n, c_pad_int(y as i64, w))?;
            }
            DCH_RM | DCH_rm => {
                if tm.mon == 0 && tm.year == 0 {
                    continue;
                }
                let months = if id == DCH_rm {
                    &WORDS_RM_MONTHS_LOWER
                } else {
                    &WORDS_RM_MONTHS_UPPER
                };
                let mon: i32 = if tm.mon == 0 {
                    if tm.year >= 0 { 0 } else { 11 }
                } else if tm.mon < 0 {
                    -(tm.mon + 1)
                } else {
                    12 - tm.mon
                };
                let mut s = months[mon as usize].to_string();
                if !has_fm(n) {
                    s = c_pad_right(&s, 4);
                }
                b.extend_from_slice(s.as_bytes());
            }
            DCH_W => put_num(
                &mut b,
                n,
                (tm.mday.wrapping_sub(1) / 7).wrapping_add(1).to_string(),
            )?,
            DCH_J => put_num(&mut b, n, pg_date2j(tm.year, tm.mon, tm.mday).to_string())?,
            _ => {}
        }
    }
    Ok(bytes_to_string(b))
}

/// Break down a wall-clock microsecond reading (PostgreSQL's timestamp2tm plus the wday and yday
/// to_char computes). `fsec` is the non-negative microsecond of the second.
fn tm_from_wall(micros: i64) -> (FmtTm, i64) {
    let (y, mo, d, h, mi, s, us) = crate::timestamp::civil_from_micros(micros);
    let mut tm = FmtTm {
        sec: s as i32,
        min: mi as i32,
        hour: h as i64,
        mday: d as i32,
        mon: mo as i32,
        year: y as i32,
        ..FmtTm::default()
    };
    let thisdate = pg_date2j(tm.year, tm.mon, tm.mday);
    // PostgreSQL's (thisdate + 1) % 7; a floor modulo beyond its range, where a Julian day is
    // negative.
    tm.wday = (thisdate as i64 + 1).rem_euclid(7) as i32;
    tm.yday = thisdate
        .wrapping_sub(pg_date2j(tm.year, 1, 1))
        .wrapping_add(1);
    (tm, us as i64)
}

/// formatting.c's datetime_to_char_body: an empty template is NULL.
fn datetime_to_char(
    tm: &FmtTm,
    fsec: i64,
    tzn: Option<&str>,
    tmpl: &str,
    is_interval: bool,
) -> Result<Option<String>> {
    if tmpl.is_empty() {
        return Ok(None);
    }
    let nodes = parse_format(tmpl.as_bytes(), true, &mut NumDesc::default())?;
    Ok(Some(dch_to_char(&nodes, is_interval, tm, fsec, tzn)?))
}

fn is_infinite_ts(micros: i64) -> bool {
    micros == crate::timestamp::POS_INFINITY || micros == crate::timestamp::NEG_INFINITY
}

/// to_char(timestamp, text): ±infinity is NULL.
pub(crate) fn to_char_timestamp(micros: i64, tmpl: &str) -> Result<Option<String>> {
    if tmpl.is_empty() || is_infinite_ts(micros) {
        return Ok(None);
    }
    let (tm, fsec) = tm_from_wall(micros);
    datetime_to_char(&tm, fsec, None, tmpl, false)
}

/// to_char(timestamptz, text): the instant decomposed in `zone`, with its offset (OF, TZH, TZM) and
/// abbreviation (TZ). A fixed-offset zone other than UTC has no abbreviation, as PostgreSQL's
/// ±HH:MM zone spelling has none. ±infinity is NULL.
pub(crate) fn to_char_timestamptz(
    instant: i64,
    zone: &ZoneRef,
    tmpl: &str,
    m: &mut Meter,
) -> Result<Option<String>> {
    if tmpl.is_empty() || is_infinite_ts(instant) {
        return Ok(None);
    }
    m.charge(COSTS.timezone);
    m.guard()?;
    let local = crate::timezone::instant_to_local_micros(zone, instant)
        .ok_or_else(crate::timestamp::zone_shift_out_of_range)?;
    let off = crate::timezone::offset_at_ref(zone, instant.div_euclid(1_000_000));
    let (mut tm, fsec) = tm_from_wall(local);
    tm.gmtoff = off.utoff as i64;
    let tzn = match zone {
        ZoneRef::Fixed(0) => "UTC".to_string(),
        ZoneRef::Fixed(_) => String::new(),
        ZoneRef::Zone(_) => off.abbrev,
    };
    datetime_to_char(&tm, fsec, Some(&tzn), tmpl, false)
}

/// to_char(interval, text) — PostgreSQL's interval2itm decomposition, with yday the approximate
/// span in days.
pub(crate) fn to_char_interval(iv: &Interval, tmpl: &str) -> Result<Option<String>> {
    if tmpl.is_empty() {
        return Ok(None);
    }
    let mut t = iv.micros;
    let hour = t / 3_600_000_000;
    t -= hour * 3_600_000_000;
    let min = t / 60_000_000;
    t -= min * 60_000_000;
    let sec = t / 1_000_000;
    let usec = t - sec * 1_000_000;
    let mut tm = FmtTm {
        sec: sec as i32,
        min: min as i32,
        hour,
        mday: iv.days,
        mon: iv.months % 12,
        year: iv.months / 12,
        ..FmtTm::default()
    };
    tm.yday = tm
        .year
        .wrapping_mul(12)
        .wrapping_add(tm.mon)
        .wrapping_mul(30)
        .wrapping_add(tm.mday);
    datetime_to_char(&tm, usec, None, tmpl, true)
}

// --- to_timestamp / to_date (DCH_from_char + do_to_timestamp) --------------------------------

/// formatting.c's TmFromChar: the fields a template read (0 = absent).
#[derive(Default)]
struct TmFromChar {
    mode: u8,
    hh: i32,
    pm: i32,
    mi: i32,
    ss: i32,
    ssss: i32,
    d: i32,
    dd: i32,
    ddd: i32,
    mm: i32,
    ms: i32,
    year: i32,
    bc: i32,
    ww: i32,
    w: i32,
    cc: i32,
    j: i32,
    us: i32,
    yysz: i32,
    clock: i32,
    tzsign: i32,
    tzh: i32,
    tzm: i32,
    ff: i32,
}

/// formatting.c's hint on a fixed-width field the source string cannot fill.
const FM_HINT: &str = "If your source string is not fixed-width, try using the \"FM\" modifier.";

impl TmFromChar {
    fn set_mode(&mut self, mode: u8) -> Result<()> {
        if mode != MODE_NONE {
            if self.mode == MODE_NONE {
                self.mode = mode;
            } else if self.mode != mode {
                return Err(EngineError::new(
                    SqlState::InvalidDatetimeFormat,
                    "invalid combination of date conventions",
                )
                .hinted(
                    "Do not mix Gregorian and ISO week date conventions in a formatting template.",
                ));
            }
        }
        Ok(())
    }
}

/// formatting.c's from_char_set_int: a field may be set twice only to one value.
fn from_char_set_int(dest: &mut i32, value: i32, n: &FmtNode) -> Result<()> {
    if *dest != 0 && *dest != value {
        return Err(EngineError::new(
            SqlState::InvalidDatetimeFormat,
            format!(
                "conflicting values for \"{}\" field in formatting string",
                n.key().name
            ),
        )
        .with_detail(Some(
            "This value contradicts a previous setting for the same field type.".to_string(),
        )));
    }
    *dest = value;
    Ok(())
}

/// C's strtol(s + start, &end, 10): skip whitespace, an optional sign, then digits; no digits leaves
/// end at start. The third value reports a saturated overflow.
fn c_strtol(s: &[u8], start: usize) -> (i64, usize, bool) {
    let mut i = start;
    while i < s.len() && is_space_c(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let ds = i;
    let mut acc: u64 = 0;
    let mut limit = i64::MAX as u64;
    if neg {
        limit += 1;
    }
    let mut over = false;
    while i < s.len() && is_digit_c(s[i]) {
        let d = (s[i] - b'0') as u64;
        if !over && acc > (limit - d) / 10 {
            over = true;
        }
        if !over {
            acc = acc * 10 + d;
        }
        i += 1;
    }
    if i == ds {
        return (0, start, false);
    }
    if over {
        return if neg {
            (i64::MIN, i, true)
        } else {
            (i64::MAX, i, true)
        };
    }
    if neg {
        // acc ≤ 2^63 here; the two's-complement negation is exact (2^63 → i64::MIN).
        return ((acc as i64).wrapping_neg(), i, false);
    }
    (acc as i64, i, false)
}

/// DCH_from_char's state: the input, the position, and the node list.
struct DchReader<'n, 'a> {
    input: &'a [u8],
    pos: usize,
    nodes: &'n [FmtNode<'a>],
}

impl DchReader<'_, '_> {
    /// formatting.c's is_next_separator: whether the field at node `i` is followed by a non-digit,
    /// so a fixed-width read may take as many digits as are present.
    fn is_next_separator(&self, i: usize) -> bool {
        let n = &self.nodes[i];
        if n.typ == FN_END {
            return false;
        }
        if n.typ == FN_ACTION && n.suffix & (SUFFIX_TH | SUFFIX_th) != 0 {
            return true;
        }
        let next = &self.nodes[i + 1];
        if next.typ == FN_END {
            return true;
        }
        if next.typ == FN_ACTION {
            return !next.key().is_digit;
        }
        if next.chars.len() == 1 && is_digit_c(next.chars[0]) {
            return false;
        }
        true
    }

    /// formatting.c's from_char_parse_int_len: read an integer field of width `ln` (any width in
    /// fill mode or before a separator). `dest` may be `None` (the value is checked, then dropped).
    fn parse_int_len(&mut self, dest: Option<&mut i32>, ln: usize, i: usize) -> Result<usize> {
        let n = self.nodes[i];
        let name = n.key().name;
        let init = self.pos;
        while self.pos < self.input.len() && is_space_c(self.input[self.pos]) {
            self.pos += 1;
        }
        let end = (self.pos + ln).min(self.input.len());
        let cp = &self.input[self.pos..end];
        let used = self.input.len() - self.pos;
        let (result, erange) = if n.suffix & SUFFIX_FM != 0 || self.is_next_separator(i) {
            let (result, e, erange) = c_strtol(self.input, init);
            self.pos = e;
            (result, erange)
        } else {
            if used < ln {
                return Err(EngineError::new(
                    SqlState::InvalidDatetimeFormat,
                    format!("source string too short for \"{name}\" formatting field"),
                )
                .with_detail(Some(format!(
                    "Field requires {ln} characters, but only {used} remain."
                )))
                .hinted(FM_HINT));
            }
            let (result, last, erange) = c_strtol(cp, 0);
            if last > 0 && last < ln {
                return Err(EngineError::new(
                    SqlState::InvalidDatetimeFormat,
                    format!("invalid value \"{}\" for \"{name}\"", lossy(cp)),
                )
                .with_detail(Some(format!(
                    "Field requires {ln} characters, but only {last} could be parsed."
                )))
                .hinted(FM_HINT));
            }
            self.pos += last;
            (result, erange)
        };
        if self.pos == init {
            return Err(EngineError::new(
                SqlState::InvalidDatetimeFormat,
                format!("invalid value \"{}\" for \"{name}\"", lossy(cp)),
            )
            .with_detail(Some("Value must be an integer.".to_string())));
        }
        if erange || result < i32::MIN as i64 || result > i32::MAX as i64 {
            return Err(EngineError::new(
                SqlState::DatetimeFieldOverflow,
                format!("value for \"{name}\" in source string is out of range"),
            )
            .with_detail(Some(format!(
                "Value must be in the range {} to {}.",
                i32::MIN,
                i32::MAX
            ))));
        }
        if let Some(dest) = dest {
            from_char_set_int(dest, result as i32, &n)?;
        }
        Ok(self.pos - init)
    }

    fn parse_int(&mut self, dest: Option<&mut i32>, i: usize) -> Result<usize> {
        let ln = self.nodes[i].key().name.len();
        self.parse_int_len(dest, ln, i)
    }

    /// formatting.c's from_char_seq_search. The TM (translated) form searches the same English
    /// words — jed's only locale is C — with ASCII case folding.
    fn seq_search(&mut self, words: &[&str], i: usize) -> Result<usize> {
        let rest = &self.input[self.pos..];
        let found = seq_search_ascii(rest, words);
        let Some((idx, ln)) = found else {
            let mut cp = rest;
            if let Some(k) = cp.iter().position(|&c| is_space_c(c)) {
                cp = &cp[..k];
            }
            return Err(EngineError::new(
                SqlState::InvalidDatetimeFormat,
                format!(
                    "invalid value \"{}\" for \"{}\"",
                    lossy(cp),
                    self.nodes[i].key().name
                ),
            )
            .with_detail(Some(
                "The given value did not match any of the allowed values for this field."
                    .to_string(),
            )));
        };
        self.pos += ln;
        Ok(idx)
    }

    /// formatting.c's SKIP_THth: an ordinal suffix on a field skips two input characters.
    fn skip_th(&mut self, suffix: u8) {
        if suffix & (SUFFIX_TH | SUFFIX_th) != 0 {
            for _ in 0..2 {
                if self.pos < self.input.len() {
                    self.pos += pg_mblen(self.input, self.pos);
                }
            }
        }
    }
}

/// formatting.c's seq_search_ascii: the first word that, ASCII case-insensitively, is a prefix of
/// `s`; its index and length.
fn seq_search_ascii(s: &[u8], words: &[&str]) -> Option<(usize, usize)> {
    if s.is_empty() {
        return None;
    }
    for (wi, w) in words.iter().enumerate() {
        let w = w.as_bytes();
        if w.len() <= s.len() && s[..w.len()].eq_ignore_ascii_case(w) {
            if w.is_empty() {
                return None;
            }
            return Some((wi, w.len()));
        }
    }
    None
}

/// sscanf(s, "%d,%03d%n"): the matched count, both numbers, and the bytes consumed.
fn scan_yyyy(s: &[u8]) -> (u8, i32, i32, usize) {
    let (v, e, _) = c_strtol(s, 0);
    if e == 0 {
        return (0, 0, 0, 0);
    }
    let millennia = v as i32;
    if e >= s.len() || s[e] != b',' {
        return (1, millennia, 0, 0);
    }
    let mut i = e + 1;
    while i < s.len() && is_space_c(s[i]) {
        i += 1;
    }
    let end = (i + 3).min(s.len());
    let (v2, e2, _) = c_strtol(&s[..end], i);
    if e2 == i {
        return (1, millennia, 0, 0);
    }
    (2, millennia, v2 as i32, e2)
}

/// formatting.c's DCH_from_char (non-standard mode).
fn dch_from_char(nodes: &[FmtNode], input: &[u8], out: &mut TmFromChar) -> Result<()> {
    let mut r = DchReader {
        input,
        pos: 0,
        nodes,
    };
    let mut fx_mode = false;
    let mut extra_skip: i64 = 0;
    let mut i = 0;
    while nodes[i].typ != FN_END && r.pos < input.len() {
        let n = nodes[i];
        let is_fx = n.typ == FN_ACTION && n.key().id == DCH_FX;
        if !fx_mode && !is_fx && (n.typ == FN_ACTION || i == 0) {
            while r.pos < input.len() && is_space_c(input[r.pos]) {
                r.pos += 1;
                extra_skip += 1;
            }
        }
        if n.typ == FN_SPACE || n.typ == FN_SEPARATOR {
            if !fx_mode {
                extra_skip -= 1;
                if r.pos < input.len()
                    && (is_space_c(input[r.pos]) || is_separator_char(input[r.pos]))
                {
                    r.pos += 1;
                    extra_skip += 1;
                }
            } else if r.pos < input.len() {
                r.pos += pg_mblen(input, r.pos);
            }
            i += 1;
            continue;
        } else if n.typ != FN_ACTION {
            if !fx_mode && extra_skip > 0 {
                extra_skip -= 1;
            } else if r.pos < input.len() {
                r.pos += pg_mblen(input, r.pos);
            }
            i += 1;
            continue;
        }
        let key = n.key();
        out.set_mode(key.date_mode)?;
        let id = key.id;
        match id {
            DCH_FX => fx_mode = true,
            DCH_A_M | DCH_P_M | DCH_a_m | DCH_p_m | DCH_AM | DCH_PM | DCH_am | DCH_pm => {
                let words: &[&str] = if matches!(id, DCH_A_M | DCH_P_M | DCH_a_m | DCH_p_m) {
                    &WORDS_AMPM_LONG
                } else {
                    &WORDS_AMPM
                };
                let v = r.seq_search(words, i)?;
                from_char_set_int(&mut out.pm, (v % 2) as i32, &n)?;
                out.clock = 12;
            }
            DCH_HH | DCH_HH12 => {
                r.parse_int_len(Some(&mut out.hh), 2, i)?;
                out.clock = 12;
                r.skip_th(n.suffix);
            }
            DCH_HH24 => {
                r.parse_int_len(Some(&mut out.hh), 2, i)?;
                r.skip_th(n.suffix);
            }
            DCH_MI => {
                r.parse_int(Some(&mut out.mi), i)?;
                r.skip_th(n.suffix);
            }
            DCH_SS => {
                r.parse_int(Some(&mut out.ss), i)?;
                r.skip_th(n.suffix);
            }
            DCH_MS => {
                let ln = r.parse_int_len(Some(&mut out.ms), 3, i)?;
                match ln {
                    1 => out.ms = out.ms.wrapping_mul(100),
                    2 => out.ms = out.ms.wrapping_mul(10),
                    _ => {}
                }
                r.skip_th(n.suffix);
            }
            DCH_FF1 | DCH_FF2 | DCH_FF3 | DCH_FF4 | DCH_FF5 | DCH_FF6 | DCH_US => {
                let mut w = 6usize;
                if id != DCH_US {
                    out.ff = (id - DCH_FF1 + 1) as i32;
                    w = out.ff as usize;
                }
                let ln = r.parse_int_len(Some(&mut out.us), w, i)?;
                let mul = match ln {
                    1 => 100000,
                    2 => 10000,
                    3 => 1000,
                    4 => 100,
                    5 => 10,
                    _ => 1,
                };
                out.us = out.us.wrapping_mul(mul);
                r.skip_th(n.suffix);
            }
            DCH_SSSS => {
                r.parse_int(Some(&mut out.ssss), i)?;
                r.skip_th(n.suffix);
            }
            DCH_tz | DCH_TZ | DCH_OF | DCH_TZH => {
                if (id == DCH_tz || id == DCH_TZ) && r.pos < input.len() && is_alpha_c(input[r.pos])
                {
                    // A zone abbreviation: jed has no abbreviation table (timezones.md §15).
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "time zone abbreviations are not supported in to_timestamp input",
                    ));
                }
                // OF / TZ read TZH[:TZM]; a sign may have been skipped as a separator (TZH).
                if r.pos < input.len()
                    && (input[r.pos] == b'+' || input[r.pos] == b'-' || input[r.pos] == b' ')
                {
                    out.tzsign = if input[r.pos] == b'-' { -1 } else { 1 };
                    r.pos += 1;
                } else if extra_skip > 0 && r.pos > 0 && input[r.pos - 1] == b'-' {
                    out.tzsign = -1;
                } else {
                    out.tzsign = 1;
                }
                r.parse_int_len(Some(&mut out.tzh), 2, i)?;
                if id != DCH_TZH && r.pos < input.len() && input[r.pos] == b':' {
                    r.pos += 1;
                    r.parse_int_len(Some(&mut out.tzm), 2, i)?;
                }
            }
            DCH_TZM => {
                if out.tzsign == 0 {
                    out.tzsign = 1;
                }
                r.parse_int_len(Some(&mut out.tzm), 2, i)?;
            }
            DCH_A_D | DCH_B_C | DCH_a_d | DCH_b_c | DCH_AD | DCH_BC | DCH_ad | DCH_bc => {
                let words: &[&str] = if matches!(id, DCH_A_D | DCH_B_C | DCH_a_d | DCH_b_c) {
                    &WORDS_ADBC_LONG
                } else {
                    &WORDS_ADBC
                };
                let v = r.seq_search(words, i)?;
                from_char_set_int(&mut out.bc, (v % 2) as i32, &n)?;
            }
            DCH_MONTH | DCH_Month | DCH_month => {
                let v = r.seq_search(&WORDS_MONTHS_FULL, i)?;
                from_char_set_int(&mut out.mm, v as i32 + 1, &n)?;
            }
            DCH_MON | DCH_Mon | DCH_mon => {
                let v = r.seq_search(&WORDS_MONTHS_ABBREV, i)?;
                from_char_set_int(&mut out.mm, v as i32 + 1, &n)?;
            }
            DCH_MM => {
                r.parse_int(Some(&mut out.mm), i)?;
                r.skip_th(n.suffix);
            }
            DCH_DAY | DCH_Day | DCH_day | DCH_DY | DCH_Dy | DCH_dy => {
                let words: &[&str] = if matches!(id, DCH_DY | DCH_Dy | DCH_dy) {
                    &WORDS_DAYS_ABBREV
                } else {
                    &WORDS_DAYS_FULL
                };
                let v = r.seq_search(words, i)?;
                from_char_set_int(&mut out.d, v as i32, &n)?;
                out.d = out.d.wrapping_add(1);
            }
            DCH_DDD => {
                r.parse_int(Some(&mut out.ddd), i)?;
                r.skip_th(n.suffix);
            }
            DCH_IDDD => {
                r.parse_int_len(Some(&mut out.ddd), 3, i)?;
                r.skip_th(n.suffix);
            }
            DCH_DD => {
                r.parse_int(Some(&mut out.dd), i)?;
                r.skip_th(n.suffix);
            }
            DCH_D => {
                r.parse_int(Some(&mut out.d), i)?;
                r.skip_th(n.suffix);
            }
            DCH_ID => {
                r.parse_int_len(Some(&mut out.d), 1, i)?;
                // Shift to the Gregorian numbering, Sunday = 1.
                out.d = out.d.wrapping_add(1);
                if out.d > 7 {
                    out.d = 1;
                }
                r.skip_th(n.suffix);
            }
            DCH_WW | DCH_IW => {
                r.parse_int(Some(&mut out.ww), i)?;
                r.skip_th(n.suffix);
            }
            DCH_Q => {
                // Q is read and checked but ignored (PostgreSQL).
                r.parse_int(None, i)?;
                r.skip_th(n.suffix);
            }
            DCH_CC => {
                r.parse_int(Some(&mut out.cc), i)?;
                r.skip_th(n.suffix);
            }
            DCH_Y_YYY => {
                let (matched, millennia, years, nch) = scan_yyyy(&input[r.pos..]);
                if matched < 2 {
                    return Err(EngineError::new(
                        SqlState::InvalidDatetimeFormat,
                        format!("invalid value \"{}\" for \"Y,YYY\"", lossy(&input[r.pos..])),
                    ));
                }
                let m64 = millennia as i64 * 1000;
                let y64 = years as i64 + m64;
                if m64 != m64 as i32 as i64 || y64 != y64 as i32 as i64 {
                    return Err(EngineError::new(
                        SqlState::DatetimeFieldOverflow,
                        "value for \"Y,YYY\" in source string is out of range",
                    ));
                }
                from_char_set_int(&mut out.year, y64 as i32, &n)?;
                out.yysz = 4;
                r.pos += nch;
                r.skip_th(n.suffix);
            }
            DCH_YYYY | DCH_IYYY => {
                r.parse_int(Some(&mut out.year), i)?;
                out.yysz = 4;
                r.skip_th(n.suffix);
            }
            DCH_YYY | DCH_IYY | DCH_YY | DCH_IY | DCH_Y | DCH_I => {
                let ln = r.parse_int(Some(&mut out.year), i)?;
                if ln < 4 {
                    out.year = adjust_partial_year(out.year);
                }
                out.yysz = key.name.len() as i32;
                r.skip_th(n.suffix);
            }
            DCH_RM | DCH_rm => {
                let v = r.seq_search(&WORDS_RM_MONTHS_LOWER, i)?;
                from_char_set_int(&mut out.mm, 12 - v as i32, &n)?;
            }
            DCH_W => {
                r.parse_int(Some(&mut out.w), i)?;
                r.skip_th(n.suffix);
            }
            DCH_J => {
                r.parse_int(Some(&mut out.j), i)?;
                r.skip_th(n.suffix);
            }
            _ => {}
        }
        // Ignore the spaces after a field.
        if !fx_mode {
            extra_skip = 0;
            while r.pos < input.len() && is_space_c(input[r.pos]) {
                r.pos += 1;
                extra_skip += 1;
            }
        }
        i += 1;
    }
    Ok(())
}

/// formatting.c's adjust_partial_year_to_2020.
fn adjust_partial_year(year: i32) -> i32 {
    if year < 70 {
        year + 2000
    } else if year < 100 {
        year + 1900
    } else if year < 520 {
        year + 2000
    } else if year < 1000 {
        year + 1000
    } else {
        year
    }
}

/// The broken-down result of do_to_timestamp (PostgreSQL's pg_tm subset).
#[derive(Clone, Copy)]
struct PgTm {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: i32,
}

/// What do_to_timestamp read: the fields, the microseconds, the fractional precision FFn asked for
/// (0 = none), and the explicit zone offset (east of UTC) if the template read one.
struct TimestampParts {
    tm: PgTm,
    fsec: i32,
    fprec: i32,
    tz_east: Option<i64>,
}

fn field_value_overflow(input: &str) -> EngineError {
    EngineError::new(
        SqlState::DatetimeFieldOverflow,
        format!("date/time field value out of range: \"{input}\""),
    )
}

/// formatting.c's do_to_timestamp.
fn to_timestamp_parts(input: &str, tmpl: &str) -> Result<TimestampParts> {
    let mut tmfc = TmFromChar::default();
    let mut tm = PgTm {
        year: 0,
        mon: 1,
        mday: 1,
        hour: 0,
        min: 0,
        sec: 0,
    };
    if !tmpl.is_empty() {
        let nodes = parse_format(tmpl.as_bytes(), true, &mut NumDesc::default())?;
        dch_from_char(&nodes, input.as_bytes(), &mut tmfc)?;
    }
    const M_YEAR: u8 = 1;
    const M_MONTH: u8 = 2;
    const M_DAY: u8 = 4;
    let overflow = || field_value_overflow(input);
    let mut fmask = 0u8;
    if tmfc.ssss != 0 {
        let mut x = tmfc.ssss;
        tm.hour = x / 3600;
        x %= 3600;
        tm.min = x / 60;
        x %= 60;
        tm.sec = x;
    }
    if tmfc.ss != 0 {
        tm.sec = tmfc.ss;
    }
    if tmfc.mi != 0 {
        tm.min = tmfc.mi;
    }
    if tmfc.hh != 0 {
        tm.hour = tmfc.hh;
    }
    if tmfc.clock == 12 {
        if tm.hour < 1 || tm.hour > 12 {
            return Err(EngineError::new(
                SqlState::InvalidDatetimeFormat,
                format!("hour \"{}\" is invalid for the 12-hour clock", tm.hour),
            )
            .hinted("Use the 24-hour clock, or give an hour between 1 and 12."));
        }
        if tmfc.pm != 0 && tm.hour < 12 {
            tm.hour += 12;
        } else if tmfc.pm == 0 && tm.hour == 12 {
            tm.hour = 0;
        }
    }
    if tmfc.year != 0 {
        // CC with YY (or Y) puts the two low-order digits in that century; a 4-digit year wins.
        if tmfc.cc != 0 && tmfc.yysz <= 2 {
            if tmfc.bc != 0 {
                tmfc.cc = tmfc.cc.wrapping_neg();
            }
            tm.year = tmfc.year % 100;
            if tm.year != 0 {
                let y = if tmfc.cc >= 0 {
                    tmfc.cc
                        .wrapping_sub(1)
                        .checked_mul(100)
                        .and_then(|tmp| tm.year.checked_add(tmp))
                } else {
                    tmfc.cc
                        .wrapping_add(1)
                        .checked_mul(100)
                        .and_then(|tmp| tmp.checked_add(-tm.year))
                        .and_then(|tmp2| tmp2.checked_add(1))
                };
                tm.year = y.ok_or_else(overflow)?;
            } else {
                // The century year of a date ending in "00".
                let adj = if tmfc.cc < 0 { 1 } else { 0 };
                tm.year = tmfc.cc.wrapping_mul(100).wrapping_add(adj);
            }
        } else {
            tm.year = tmfc.year;
            if tmfc.bc != 0 {
                tm.year = tm.year.wrapping_neg();
            }
            if tm.year < 0 {
                tm.year += 1;
            }
        }
        fmask |= M_YEAR;
    } else if tmfc.cc != 0 {
        // The first year of the century.
        if tmfc.bc != 0 {
            tmfc.cc = tmfc.cc.wrapping_neg();
        }
        let y = if tmfc.cc >= 0 {
            tmfc.cc.wrapping_sub(1).checked_mul(100)
        } else {
            tmfc.cc.checked_mul(100)
        };
        tm.year = y.and_then(|y| y.checked_add(1)).ok_or_else(overflow)?;
        fmask |= M_YEAR;
    }
    if tmfc.j != 0 {
        (tm.year, tm.mon, tm.mday) = pg_j2date(tmfc.j);
        fmask |= M_YEAR | M_MONTH | M_DAY;
    }
    if tmfc.ww != 0 {
        if tmfc.mode == MODE_ISOWEEK {
            // No day of week leaves the date at the start (Monday) of the ISO week.
            if tmfc.d != 0 {
                let mut jday = pg_isoweek2j(tm.year, tmfc.ww);
                if tmfc.d > 1 {
                    jday = jday.wrapping_add(tmfc.d.wrapping_sub(2));
                } else {
                    jday = jday.wrapping_add(6);
                }
                (tm.year, tm.mon, tm.mday) = pg_j2date(jday);
            } else {
                (tm.year, tm.mon, tm.mday) = pg_j2date(pg_isoweek2j(tm.year, tmfc.ww));
            }
            fmask |= M_YEAR | M_MONTH | M_DAY;
        } else {
            tmfc.ddd = tmfc
                .ww
                .checked_add(-1)
                .and_then(|a| a.checked_mul(7))
                .and_then(|b| b.checked_add(1))
                .ok_or_else(overflow)?;
        }
    }
    if tmfc.w != 0 {
        tmfc.dd = tmfc
            .w
            .checked_add(-1)
            .and_then(|a| a.checked_mul(7))
            .and_then(|b| b.checked_add(1))
            .ok_or_else(overflow)?;
    }
    if tmfc.dd != 0 {
        tm.mday = tmfc.dd;
        fmask |= M_DAY;
    }
    if tmfc.mm != 0 {
        tm.mon = tmfc.mm;
        fmask |= M_MONTH;
    }
    if tmfc.ddd != 0 && (tm.mon <= 1 || tm.mday <= 1) {
        // The month and day come from the day of the year, Gregorian or ISO.
        if tm.year == 0 && tmfc.bc == 0 {
            return Err(EngineError::new(
                SqlState::InvalidDatetimeFormat,
                "cannot calculate day of year without year information",
            ));
        }
        if tmfc.mode == MODE_ISOWEEK {
            let j0 = pg_isoweek2j(tm.year, 1).wrapping_sub(1);
            (tm.year, tm.mon, tm.mday) = pg_j2date(j0.wrapping_add(tmfc.ddd));
            fmask |= M_YEAR | M_MONTH | M_DAY;
        } else {
            const YSUM: [[i32; 13]; 2] = [
                [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
                [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
            ];
            let y = &YSUM[if pg_is_leap(tm.year) { 1 } else { 0 }];
            let mut i = 1usize;
            while i <= 12 {
                if tmfc.ddd <= y[i] {
                    break;
                }
                i += 1;
            }
            if tm.mon <= 1 {
                tm.mon = i as i32;
            }
            if tm.mday <= 1 {
                tm.mday = tmfc.ddd.wrapping_sub(y[i - 1]);
            }
            fmask |= M_MONTH | M_DAY;
        }
    }
    let mut fsec: i32 = 0;
    if tmfc.ms != 0 {
        fsec = tmfc
            .ms
            .checked_mul(1000)
            .and_then(|tmp| fsec.checked_add(tmp))
            .ok_or_else(overflow)?;
    }
    if tmfc.us != 0 {
        fsec = fsec.wrapping_add(tmfc.us);
    }
    let fprec = tmfc.ff;
    if fmask != 0 {
        // PostgreSQL's ValidateDate (the year was already adjusted for AD/BC).
        let mut bad = (fmask & M_MONTH != 0 && (tm.mon < 1 || tm.mon > 12))
            || (fmask & M_DAY != 0 && (tm.mday < 1 || tm.mday > 31));
        if !bad && fmask == M_YEAR | M_MONTH | M_DAY {
            let leap = if pg_is_leap(tm.year) { 1 } else { 0 };
            bad = tm.mday > PG_DAY_TAB[leap][(tm.mon - 1) as usize];
        }
        if bad {
            return Err(overflow());
        }
    }
    if tm.hour < 0
        || tm.hour >= 24
        || tm.min < 0
        || tm.min >= 60
        || tm.sec < 0
        || tm.sec >= 60
        || !(0..1_000_000).contains(&fsec)
    {
        return Err(overflow());
    }
    let mut tz_east = None;
    if tmfc.tzsign != 0 {
        if tmfc.tzh < 0 || tmfc.tzh > 15 || tmfc.tzm < 0 || tmfc.tzm >= 60 {
            return Err(EngineError::new(
                SqlState::InvalidTimeZoneDisplacementValue,
                format!("time zone displacement out of range: \"{input}\""),
            ));
        }
        let mut east = (tmfc.tzh as i64 * 60 + tmfc.tzm as i64) * 60;
        if tmfc.tzsign < 0 {
            east = -east;
        }
        tz_east = Some(east);
    }
    Ok(TimestampParts {
        tm,
        fsec,
        fprec,
        tz_east,
    })
}

/// The wall-clock microsecond reading of the broken-down fields, or `None` when it leaves the
/// finite timestamp range.
fn wall_from_tm(tm: &PgTm, fsec: i32) -> Option<i64> {
    let days = crate::timestamp::days_from_civil(tm.year as i64, tm.mon as i64, tm.mday as i64);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(tm.hour as i64 * 3600 + tm.min as i64 * 60 + tm.sec as i64)?;
    let t = secs.checked_mul(1_000_000)?.checked_add(fsec as i64)?;
    if is_infinite_ts(t) {
        return None;
    }
    Some(t)
}

/// PostgreSQL's 2000-01-01 epoch, in jed's microseconds since 1970-01-01.
const PG_EPOCH_SHIFT_MICROS: i64 = 946_684_800 * 1_000_000;

/// PostgreSQL's AdjustTimestampForTypmod for 0 < prec < 6: round half away from zero to `prec`
/// fractional digits, the sign taken relative to PostgreSQL's 2000-01-01 epoch.
fn round_to_precision(t: i64, prec: i32) -> Option<i64> {
    if prec <= 0 || prec >= 6 {
        return Some(t);
    }
    let mut scale: i64 = 1;
    for _ in prec..6 {
        scale *= 10;
    }
    let offset = scale / 2;
    let mut pg = t.checked_sub(PG_EPOCH_SHIFT_MICROS)?;
    if pg >= 0 {
        pg = pg.wrapping_add(offset) / scale * scale;
    } else {
        pg = (pg.wrapping_neg().wrapping_add(offset) / scale)
            .wrapping_mul(scale)
            .wrapping_neg();
    }
    let r = pg.checked_add(PG_EPOCH_SHIFT_MICROS)?;
    if is_infinite_ts(r) {
        return None;
    }
    Some(r)
}

/// to_timestamp(text, text) → timestamptz: an explicit TZH / TZM / OF offset fixes the instant;
/// otherwise the wall clock is read in `zone` (one timezone unit).
pub(crate) fn to_timestamp_text(
    input: &str,
    tmpl: &str,
    zone: &ZoneRef,
    m: &mut Meter,
) -> Result<i64> {
    let parts = to_timestamp_parts(input, tmpl)?;
    let out_of_range =
        || EngineError::new(SqlState::DatetimeFieldOverflow, "timestamp out of range");
    let wall = wall_from_tm(&parts.tm, parts.fsec).ok_or_else(out_of_range)?;
    let inst = match parts.tz_east {
        Some(east) => wall
            .checked_sub(east * 1_000_000)
            .filter(|&v| !is_infinite_ts(v)),
        None => {
            m.charge(COSTS.timezone);
            m.guard()?;
            crate::timezone::local_to_instant_micros(zone, wall)
        }
    };
    let mut inst = inst.ok_or_else(out_of_range)?;
    if parts.fprec != 0 {
        inst = round_to_precision(inst, parts.fprec).ok_or_else(out_of_range)?;
    }
    Ok(inst)
}

/// to_date(text, text) → date.
pub(crate) fn to_date_text(input: &str, tmpl: &str) -> Result<i32> {
    let parts = to_timestamp_parts(input, tmpl)?;
    let tm = parts.tm;
    let days = crate::timestamp::days_from_civil(tm.year as i64, tm.mon as i64, tm.mday as i64);
    if days <= crate::date::NEG_INFINITY as i64 || days >= crate::date::POS_INFINITY as i64 {
        return Err(EngineError::new(
            SqlState::DatetimeFieldOverflow,
            format!("date out of range: \"{input}\""),
        ));
    }
    Ok(days as i32)
}

/// to_timestamp(double precision): Unix-epoch seconds, PostgreSQL's float8_timestamptz — computed
/// in PostgreSQL's 2000 epoch with its float operations, so the rounding matches to the
/// microsecond.
pub(crate) fn to_timestamp_float(secs: f64) -> Result<i64> {
    if secs.is_nan() {
        return Err(EngineError::new(
            SqlState::DatetimeFieldOverflow,
            "timestamp cannot be NaN",
        ));
    }
    if secs == f64::INFINITY {
        return Ok(crate::timestamp::POS_INFINITY);
    }
    if secs == f64::NEG_INFINITY {
        return Ok(crate::timestamp::NEG_INFINITY);
    }
    let too_far = || {
        EngineError::new(
            SqlState::DatetimeFieldOverflow,
            format!(
                "timestamp out of range: \"{}\"",
                crate::value::render_float_pg(secs, false)
            ),
        )
    };
    // jed's range is the finite int64 microseconds, checked on the rounded count.
    let us = ((secs - 946_684_800.0) * 1_000_000.0).round_ties_even();
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&us) {
        return Err(too_far());
    }
    let r = (us as i64)
        .checked_add(PG_EPOCH_SHIFT_MICROS)
        .ok_or_else(too_far)?;
    if is_infinite_ts(r) {
        return Err(too_far());
    }
    Ok(r)
}
