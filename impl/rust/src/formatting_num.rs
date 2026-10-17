//! The number half of the data-type formatting functions — to_char over integer, decimal, and
//! float values, and to_number (spec/design/formatting.md §6–§7): PostgreSQL 18's NUMDesc_prepare,
//! NUM_processor, and the per-type to_char preparers, transcribed byte for byte. The C printf
//! conversions PostgreSQL's float paths use ("%.*f", "%+.*e") are reproduced exactly from the
//! float's exact decimal expansion (round half to even, glibc's rule), with PostgreSQL's own
//! snprintf spellings of NaN and Infinity.

// The generated pattern ids keep PostgreSQL's case-distinguished names (DCH_am, NUM_rn, …).
#![allow(non_upper_case_globals)]

use crate::cost::Meter;
use crate::decimal::Decimal;
use crate::error::{EngineError, Result, SqlState};
use crate::formatting::{
    FN_ACTION, FN_END, FmtNode, ascii_lower, bytes_to_string, c_pad_int, c_pad_left, fmt_ordinal,
    is_digit_c, is_space_c, parse_format, pg_mblen,
};
use crate::formatting_gen::*;

// The number template flags (formatting.c NUM_F_*).
const F_DECIMAL: i64 = 1 << 1;
const F_LDECIMAL: i64 = 1 << 2;
const F_ZERO: i64 = 1 << 3;
const F_BLANK: i64 = 1 << 4;
const F_FILLMODE: i64 = 1 << 5;
const F_LSIGN: i64 = 1 << 6;
const F_BRACKET: i64 = 1 << 7;
const F_MINUS: i64 = 1 << 8;
const F_PLUS: i64 = 1 << 9;
const F_ROMAN: i64 = 1 << 10;
const F_MULTI: i64 = 1 << 11;
const F_PLUS_POST: i64 = 1 << 12;
const F_MINUS_POST: i64 = 1 << 13;
const F_EEEE: i64 = 1 << 14;

const LSIGN_PRE: i64 = -1;
const LSIGN_POST: i64 = 1;
const LSIGN_NONE: i64 = 0;

// The C-locale symbols (PostgreSQL's NUM_prepare_locale under lc_numeric = C).
const LOCALE_DECIMAL: &[u8] = b".";
const LOCALE_THOUSAND: &[u8] = b",";
const LOCALE_CURRENCY: &[u8] = b" ";
const LOCALE_NEGATIVE: &[u8] = b"-";
const LOCALE_POSITIVE: &[u8] = b"+";

/// formatting.c's NUMDesc: what a number template asks for.
#[derive(Clone, Copy, Default)]
pub(crate) struct NumDesc {
    pre: i64,
    post: i64,
    lsign: i64,
    flag: i64,
    pre_lsign_num: i64,
    multi: i64,
    zero_start: i64,
    zero_end: i64,
}

fn num_syntax(msg: &str) -> EngineError {
    EngineError::new(SqlState::SyntaxError, msg)
}

impl NumDesc {
    fn has(&self, f: i64) -> bool {
        self.flag & f != 0
    }

    /// formatting.c's NUMDesc_prepare for one pattern.
    pub(crate) fn prepare(&mut self, id: u8) -> Result<()> {
        if self.has(F_EEEE) && id != NUM_E {
            return Err(num_syntax("\"EEEE\" must be the last pattern used"));
        }
        match id {
            NUM_9 => {
                if self.has(F_BRACKET) {
                    return Err(num_syntax("\"9\" must be ahead of \"PR\""));
                }
                if self.has(F_MULTI) {
                    self.multi += 1;
                } else if self.has(F_DECIMAL) {
                    self.post += 1;
                } else {
                    self.pre += 1;
                }
            }
            NUM_0 => {
                if self.has(F_BRACKET) {
                    return Err(num_syntax("\"0\" must be ahead of \"PR\""));
                }
                if !self.has(F_ZERO) && !self.has(F_DECIMAL) {
                    self.flag |= F_ZERO;
                    self.zero_start = self.pre + 1;
                }
                if !self.has(F_DECIMAL) {
                    self.pre += 1;
                } else {
                    self.post += 1;
                }
                self.zero_end = self.pre + self.post;
            }
            NUM_B => {
                if self.pre == 0 && self.post == 0 && !self.has(F_ZERO) {
                    self.flag |= F_BLANK;
                }
            }
            NUM_D | NUM_DEC => {
                if id == NUM_D {
                    self.flag |= F_LDECIMAL;
                }
                if self.has(F_DECIMAL) {
                    return Err(num_syntax("multiple decimal points"));
                }
                if self.has(F_MULTI) {
                    return Err(num_syntax("cannot use \"V\" and decimal point together"));
                }
                self.flag |= F_DECIMAL;
            }
            NUM_FM => self.flag |= F_FILLMODE,
            NUM_S => {
                if self.has(F_LSIGN) {
                    return Err(num_syntax("cannot use \"S\" twice"));
                }
                if self.has(F_PLUS) || self.has(F_MINUS) || self.has(F_BRACKET) {
                    return Err(num_syntax(
                        "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together",
                    ));
                }
                if !self.has(F_DECIMAL) {
                    self.lsign = LSIGN_PRE;
                    self.pre_lsign_num = self.pre;
                    self.flag |= F_LSIGN;
                } else if self.lsign == LSIGN_NONE {
                    self.lsign = LSIGN_POST;
                    self.flag |= F_LSIGN;
                }
            }
            NUM_MI => {
                if self.has(F_LSIGN) {
                    return Err(num_syntax("cannot use \"S\" and \"MI\" together"));
                }
                self.flag |= F_MINUS;
                if self.has(F_DECIMAL) {
                    self.flag |= F_MINUS_POST;
                }
            }
            NUM_PL => {
                if self.has(F_LSIGN) {
                    return Err(num_syntax("cannot use \"S\" and \"PL\" together"));
                }
                self.flag |= F_PLUS;
                if self.has(F_DECIMAL) {
                    self.flag |= F_PLUS_POST;
                }
            }
            NUM_SG => {
                if self.has(F_LSIGN) {
                    return Err(num_syntax("cannot use \"S\" and \"SG\" together"));
                }
                self.flag |= F_MINUS | F_PLUS;
            }
            NUM_PR => {
                if self.has(F_LSIGN) || self.has(F_PLUS) || self.has(F_MINUS) {
                    return Err(num_syntax(
                        "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
                    ));
                }
                self.flag |= F_BRACKET;
            }
            NUM_RN | NUM_rn => {
                if self.has(F_ROMAN) {
                    return Err(num_syntax("cannot use \"RN\" twice"));
                }
                self.flag |= F_ROMAN;
            }
            NUM_V => {
                if self.has(F_DECIMAL) {
                    return Err(num_syntax("cannot use \"V\" and decimal point together"));
                }
                self.flag |= F_MULTI;
            }
            NUM_E => {
                if self.has(F_EEEE) {
                    return Err(num_syntax("cannot use \"EEEE\" twice"));
                }
                if self.has(F_BLANK)
                    || self.has(F_FILLMODE)
                    || self.has(F_LSIGN)
                    || self.has(F_BRACKET)
                    || self.has(F_MINUS)
                    || self.has(F_PLUS)
                    || self.has(F_ROMAN)
                    || self.has(F_MULTI)
                {
                    return Err(num_syntax("\"EEEE\" is incompatible with other formats"));
                }
                self.flag |= F_EEEE;
            }
            _ => {}
        }
        if self.has(F_ROMAN) && self.flag & !(F_ROMAN | F_FILLMODE) != 0 {
            return Err(num_syntax("\"RN\" is incompatible with other formats"));
        }
        Ok(())
    }
}

/// formatting.c's NUMProc. For to_char, `out` is the output and `number` the digit string being
/// laid out; for to_number, `inout` is the input and `number` the digits read (`number[0]` is the
/// sign slot).
struct NumProc<'a> {
    num: &'a mut NumDesc,
    sign: u8,
    sign_wrote: bool,
    num_count: i64,
    num_in: bool,
    num_curr: i64,
    out_pre_spaces: i64,
    read_dec: bool,
    read_post: i64,
    read_pre: i64,
    number: Vec<u8>,
    number_p: usize,
    inout: &'a [u8],
    inout_p: usize,
    out: Vec<u8>,
    /// -1: none.
    last_relevant: i64,
}

fn sign_symbol(sign: u8) -> &'static [u8] {
    if sign == b'-' {
        LOCALE_NEGATIVE
    } else {
        LOCALE_POSITIVE
    }
}

impl NumProc<'_> {
    /// The number byte at `i`, 0 past the end (the C string's terminator).
    fn at(&self, i: usize) -> u8 {
        self.number.get(i).copied().unwrap_or(0)
    }

    /// The input byte at `i`, 0 past the end.
    fn input(&self, i: usize) -> u8 {
        self.inout.get(i).copied().unwrap_or(0)
    }

    fn overload(&self) -> bool {
        self.inout_p >= self.inout.len()
    }

    fn amount(&self, s: usize) -> bool {
        self.inout_p + s <= self.inout.len()
    }

    fn input_has_prefix(&self, p: &[u8]) -> bool {
        self.amount(p.len()) && self.inout[self.inout_p..].starts_with(p)
    }

    /// formatting.c's IS_PREDEC_SPACE: a lone leading zero before the decimal point.
    fn is_predec_space(&self) -> bool {
        !self.num.has(F_ZERO) && self.number_p == 0 && self.at(0) == b'0' && self.num.post != 0
    }

    fn last_relevant_is_point(&self) -> bool {
        self.last_relevant >= 0 && self.at(self.last_relevant as usize) == b'.'
    }

    /// formatting.c's NUM_numpart_to_char: write a sign, digit, or decimal point.
    fn numpart_to_char(&mut self, id: u8) {
        if self.num.has(F_ROMAN) {
            return;
        }
        self.num_in = false;
        let fill = self.num.has(F_FILLMODE);
        if !self.sign_wrote
            && (self.num_curr >= self.out_pre_spaces
                || (self.num.has(F_ZERO) && self.num.zero_start == self.num_curr))
            && (!self.is_predec_space() || self.last_relevant_is_point())
        {
            if self.num.has(F_LSIGN) {
                if self.num.lsign == LSIGN_PRE {
                    self.out.extend_from_slice(sign_symbol(self.sign));
                    self.sign_wrote = true;
                }
            } else if self.num.has(F_BRACKET) {
                self.out.push(if self.sign == b'+' { b' ' } else { b'<' });
                self.sign_wrote = true;
            } else if self.sign == b'+' {
                if !fill {
                    self.out.push(b' ');
                }
                self.sign_wrote = true;
            } else if self.sign == b'-' {
                self.out.push(b'-');
                self.sign_wrote = true;
            }
        }
        if id == NUM_9 || id == NUM_0 || id == NUM_D || id == NUM_DEC {
            if self.num_curr < self.out_pre_spaces
                && (self.num.zero_start > self.num_curr || !self.num.has(F_ZERO))
            {
                if !fill {
                    self.out.push(b' ');
                }
            } else if self.num.has(F_ZERO)
                && self.num_curr < self.out_pre_spaces
                && self.num.zero_start <= self.num_curr
            {
                self.out.push(b'0');
                self.num_in = true;
            } else {
                if self.at(self.number_p) == b'.' {
                    if !self.last_relevant_is_point() || fill {
                        self.out.extend_from_slice(LOCALE_DECIMAL);
                    }
                } else if self.last_relevant >= 0
                    && self.number_p as i64 > self.last_relevant
                    && id != NUM_0
                {
                } else if self.is_predec_space() {
                    if !fill {
                        self.out.push(b' ');
                    } else if self.last_relevant_is_point() {
                        self.out.push(b'0');
                    }
                } else {
                    let c = self.at(self.number_p);
                    self.out.push(c);
                    self.num_in = true;
                }
                if self.at(self.number_p) != 0 {
                    self.number_p += 1;
                }
            }
            let mut end = self.num_count;
            if self.out_pre_spaces != 0 {
                end += 1;
            }
            if self.num.has(F_DECIMAL) {
                end += 1;
            }
            if self.last_relevant >= 0 && self.last_relevant == self.number_p as i64 {
                end = self.num_curr;
            }
            if self.num_curr + 1 == end {
                if self.sign_wrote && self.num.has(F_BRACKET) {
                    self.out.push(if self.sign == b'+' { b' ' } else { b'>' });
                } else if self.num.has(F_LSIGN) && self.num.lsign == LSIGN_POST {
                    self.out.extend_from_slice(sign_symbol(self.sign));
                }
            }
        }
        self.num_curr += 1;
    }

    /// formatting.c's NUM_numpart_from_char: read a sign, digit, or decimal point.
    fn numpart_from_char(&mut self, id: u8) {
        if self.overload() {
            return;
        }
        if self.input(self.inout_p) == b' ' {
            self.inout_p += 1;
        }
        if self.overload() {
            return;
        }
        // A sign before the number.
        if self.number[0] == b' '
            && (id == NUM_0 || id == NUM_9)
            && self.read_pre + self.read_post == 0
        {
            if self.num.has(F_LSIGN) && self.num.lsign == LSIGN_PRE {
                if self.input_has_prefix(LOCALE_NEGATIVE) {
                    self.inout_p += LOCALE_NEGATIVE.len();
                    self.number[0] = b'-';
                } else if self.input_has_prefix(LOCALE_POSITIVE) {
                    self.inout_p += LOCALE_POSITIVE.len();
                    self.number[0] = b'+';
                }
            } else {
                let c = self.input(self.inout_p);
                if c == b'-' || (self.num.has(F_BRACKET) && c == b'<') {
                    self.number[0] = b'-';
                    self.inout_p += 1;
                } else if c == b'+' {
                    self.number[0] = b'+';
                    self.inout_p += 1;
                }
            }
        }
        if self.overload() {
            return;
        }
        let mut isread = false;
        if is_digit_c(self.input(self.inout_p)) {
            if self.read_dec && self.read_post == self.num.post {
                return;
            }
            let c = self.input(self.inout_p);
            self.number.push(c);
            self.number_p += 1;
            if self.read_dec {
                self.read_post += 1;
            } else {
                self.read_pre += 1;
            }
            isread = true;
        } else if self.num.has(F_DECIMAL) && !self.read_dec && self.input_has_prefix(LOCALE_DECIMAL)
        {
            self.inout_p += LOCALE_DECIMAL.len() - 1;
            self.number.push(b'.');
            self.number_p += 1;
            self.read_dec = true;
            isread = true;
        }
        if self.overload() {
            return;
        }
        // A sign after the last digit.
        if self.number[0] == b' ' && self.read_pre + self.read_post > 0 {
            if self.num.has(F_LSIGN)
                && isread
                && self.inout_p + 1 < self.inout.len()
                && !is_digit_c(self.input(self.inout_p + 1))
            {
                let tmp = self.inout_p;
                self.inout_p += 1;
                if self.input_has_prefix(LOCALE_NEGATIVE) {
                    self.inout_p += LOCALE_NEGATIVE.len() - 1;
                    self.number[0] = b'-';
                } else if self.input_has_prefix(LOCALE_POSITIVE) {
                    self.inout_p += LOCALE_POSITIVE.len() - 1;
                    self.number[0] = b'+';
                }
                if self.number[0] == b' ' {
                    self.inout_p = tmp;
                }
            } else if !isread
                && !self.num.has(F_LSIGN)
                && (self.num.has(F_PLUS) || self.num.has(F_MINUS))
            {
                let c = self.input(self.inout_p);
                if c == b'-' || c == b'+' {
                    self.number[0] = c;
                }
            }
        }
    }

    /// formatting.c's NUM_eat_non_data_chars: skip up to `n` input characters that are not numeric
    /// data.
    fn eat_non_data_chars(&mut self, n: usize) {
        for _ in 0..n {
            if self.overload() {
                break;
            }
            if b"0123456789.,+-".contains(&self.input(self.inout_p)) {
                break;
            }
            self.inout_p += pg_mblen(self.inout, self.inout_p);
        }
    }

    /// formatting.c's roman_to_int: a standard-form Roman numeral (1..3999), or -1.
    fn roman_to_int(&mut self) -> i64 {
        while !self.overload() && is_space_c(self.input(self.inout_p)) {
            self.inout_p += 1;
        }
        let mut chars: Vec<u8> = Vec::new();
        let mut vals: Vec<i64> = Vec::new();
        while chars.len() < 15 && !self.overload() {
            let mut c = self.input(self.inout_p);
            if c.is_ascii_lowercase() {
                c -= 32;
            }
            let v = roman_val(c);
            if v == 0 {
                break;
            }
            chars.push(c);
            vals.push(v);
            self.inout_p += 1;
        }
        if chars.is_empty() {
            return -1;
        }
        let mut result: i64 = 0;
        let mut repeat = 1;
        let (mut v_count, mut l_count, mut d_count) = (0, 0, 0);
        let mut subtraction = false;
        let mut last_sub: i64 = 0;
        let mut i = 0;
        while i < chars.len() {
            let (cur, cv) = (chars[i], vals[i]);
            if subtraction && cv >= last_sub {
                return -1;
            }
            if (v_count != 0 && cv >= 5)
                || (l_count != 0 && cv >= 50)
                || (d_count != 0 && cv >= 500)
            {
                return -1;
            }
            match cur {
                b'V' => v_count += 1,
                b'L' => l_count += 1,
                b'D' => d_count += 1,
                _ => {}
            }
            if i < chars.len() - 1 {
                let (next, nv) = (chars[i + 1], vals[i + 1]);
                if cv < nv {
                    if !valid_sub_comb(cur, next) || repeat > 1 {
                        return -1;
                    }
                    if (v_count != 0 && nv >= 5)
                        || (l_count != 0 && nv >= 50)
                        || (d_count != 0 && nv >= 500)
                    {
                        return -1;
                    }
                    match next {
                        b'V' => v_count += 1,
                        b'L' => l_count += 1,
                        b'D' => d_count += 1,
                        _ => {}
                    }
                    i += 1;
                    repeat = 1;
                    subtraction = true;
                    last_sub = cv;
                    result += nv - cv;
                } else {
                    if cur == next {
                        repeat += 1;
                        if repeat > 3 {
                            return -1;
                        }
                    } else {
                        repeat = 1;
                    }
                    result += cv;
                }
            } else {
                result += cv;
            }
            i += 1;
        }
        result
    }
}

/// formatting.c's ROMAN_VAL.
fn roman_val(c: u8) -> i64 {
    match c {
        b'I' => 1,
        b'V' => 5,
        b'X' => 10,
        b'L' => 50,
        b'C' => 100,
        b'D' => 500,
        b'M' => 1000,
        _ => 0,
    }
}

fn valid_sub_comb(cur: u8, next: u8) -> bool {
    (cur == b'I' && (next == b'V' || next == b'X'))
        || (cur == b'X' && (next == b'L' || next == b'C'))
        || (cur == b'C' && (next == b'D' || next == b'M'))
}

/// formatting.c's int_to_roman: upper-case, unpadded; 15 '#' out of 1..3999.
fn int_to_roman(number: i64) -> String {
    if !(1..=3999).contains(&number) {
        return "#".repeat(15);
    }
    let mut b = String::new();
    let s = number.to_string();
    let bytes = s.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        let d = (c - b'0') as i64 - 1;
        if d < 0 {
            continue;
        }
        let d = d as usize;
        match bytes.len() - i {
            4 => b.push_str(&"M".repeat(d + 1)),
            3 => b.push_str(WORDS_RN_HUNDREDS[d]),
            2 => b.push_str(WORDS_RN_TENS[d]),
            1 => b.push_str(WORDS_RN_UNITS[d]),
            _ => {}
        }
    }
    b
}

/// `b` up to its first NUL byte.
fn c_string(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(k) => &b[..k],
        None => b,
    }
}

/// formatting.c's get_last_relevant_decnum: the last nonzero digit after the decimal point, the
/// point itself if there is none, or -1 without a point.
fn last_relevant_decnum(num: &[u8]) -> i64 {
    let Some(p) = num.iter().position(|&c| c == b'.') else {
        return -1;
    };
    let mut result = p;
    let mut i = p + 1;
    while i < num.len() && num[i] != 0 {
        if num[i] != b'0' {
            result = i;
        }
        i += 1;
    }
    result as i64
}

/// formatting.c's NUM_processor. For to_char it returns the formatted text; for to_number the
/// digits read, with `num.post` set to the fractional digits read.
fn num_processor(
    nodes: &[FmtNode],
    num: &mut NumDesc,
    inout: &[u8],
    number: Vec<u8>,
    out_pre_spaces: i64,
    sign: u8,
    is_to_char: bool,
) -> Result<Vec<u8>> {
    if num.zero_start != 0 {
        num.zero_start -= 1;
    }
    if num.has(F_EEEE) {
        if !is_to_char {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "\"EEEE\" not supported for input",
            ));
        }
        return Ok(number);
    }
    let fill = num.has(F_FILLMODE);
    let mut np = NumProc {
        num,
        sign: 0,
        sign_wrote: false,
        num_count: 0,
        num_in: false,
        num_curr: 0,
        out_pre_spaces: 0,
        read_dec: false,
        read_post: 0,
        read_pre: 0,
        number,
        number_p: 0,
        inout,
        inout_p: 0,
        out: Vec::new(),
        last_relevant: -1,
    };
    if is_to_char {
        np.sign = sign;
        if np.num.has(F_PLUS) || np.num.has(F_MINUS) {
            np.sign_wrote = !(np.num.has(F_PLUS) && !np.num.has(F_MINUS));
        } else {
            if np.sign != b'-' && fill {
                np.num.flag &= !F_BRACKET;
            }
            np.sign_wrote = np.sign == b'+' && fill && !np.num.has(F_LSIGN);
            if np.num.lsign == LSIGN_PRE && np.num.pre == np.num.pre_lsign_num {
                np.num.lsign = LSIGN_POST;
            }
        }
    }
    np.num_count = np.num.post + np.num.pre - 1;
    if is_to_char {
        np.out_pre_spaces = out_pre_spaces;
        if fill && np.num.has(F_DECIMAL) {
            np.last_relevant = last_relevant_decnum(&np.number);
            // Keep the digits a '0' pattern asks for, but not past the end of the number.
            if np.last_relevant >= 0 && np.num.zero_end > np.out_pre_spaces {
                let mut last_zero = np.number.len() as i64 - 1;
                let z = np.num.zero_end - np.out_pre_spaces;
                if z < last_zero {
                    last_zero = z;
                }
                if np.last_relevant < last_zero {
                    np.last_relevant = last_zero;
                }
            }
        }
        if !np.sign_wrote && np.out_pre_spaces == 0 {
            np.num_count += 1;
        }
    } else {
        np.number = vec![b' '];
        np.number_p = 1;
    }
    for n in nodes {
        if n.typ == FN_END {
            break;
        }
        if !is_to_char && np.overload() {
            break;
        }
        if n.typ != FN_ACTION {
            if is_to_char {
                np.out.extend_from_slice(n.chars);
            } else {
                np.inout_p += pg_mblen(np.inout, np.inout_p);
            }
            continue;
        }
        let id = n.key().id;
        match id {
            NUM_9 | NUM_0 | NUM_DEC | NUM_D => {
                if is_to_char {
                    np.numpart_to_char(id);
                    continue;
                }
                np.numpart_from_char(id);
            }
            NUM_COMMA => {
                if is_to_char {
                    if np.num_in {
                        np.out.push(b',');
                    } else if !fill {
                        np.out.push(b' ');
                    }
                    continue;
                }
                if !np.num_in && fill {
                    continue;
                }
                if np.input(np.inout_p) != b',' {
                    continue;
                }
            }
            NUM_G => {
                if is_to_char {
                    if np.num_in {
                        np.out.extend_from_slice(LOCALE_THOUSAND);
                    } else if !fill {
                        np.out
                            .extend(std::iter::repeat_n(b' ', LOCALE_THOUSAND.len()));
                    }
                    continue;
                }
                if !np.num_in && fill {
                    continue;
                }
                if !np.input_has_prefix(LOCALE_THOUSAND) {
                    continue;
                }
                np.inout_p += LOCALE_THOUSAND.len() - 1;
            }
            NUM_L => {
                if is_to_char {
                    np.out.extend_from_slice(LOCALE_CURRENCY);
                    continue;
                }
                np.eat_non_data_chars(LOCALE_CURRENCY.len());
                continue;
            }
            NUM_RN | NUM_rn => {
                if is_to_char {
                    let tail = c_string(&np.number[np.number_p.min(np.number.len())..]);
                    let mut s = String::from_utf8_lossy(tail).into_owned();
                    if id == NUM_rn {
                        s = ascii_lower(&s);
                    }
                    if !fill {
                        s = c_pad_left(&s, 15);
                    }
                    np.out.extend_from_slice(s.as_bytes());
                    continue;
                }
                let r = np.roman_to_int();
                if r < 0 {
                    return Err(EngineError::new(
                        SqlState::InvalidTextRepresentation,
                        "invalid Roman numeral",
                    ));
                }
                let s = r.to_string();
                np.number.extend_from_slice(s.as_bytes());
                np.number_p += s.len();
                np.num.pre = s.len() as i64;
                np.num.post = 0;
                continue;
            }
            NUM_th | NUM_TH => {
                if np.num.has(F_ROMAN)
                    || np.at(0) == b'#'
                    || np.sign == b'-'
                    || np.num.has(F_DECIMAL)
                {
                    continue;
                }
                if is_to_char {
                    let th = fmt_ordinal(c_string(&np.number), id == NUM_TH)?;
                    np.out.extend_from_slice(th.as_bytes());
                    continue;
                }
                np.eat_non_data_chars(2);
                continue;
            }
            NUM_MI | NUM_PL => {
                let want = if id == NUM_PL { b'+' } else { b'-' };
                if is_to_char {
                    if np.sign == want {
                        np.out.push(want);
                    } else if !fill {
                        np.out.push(b' ');
                    }
                    continue;
                }
                if np.input(np.inout_p) != want {
                    np.eat_non_data_chars(1);
                    continue;
                }
                np.number[0] = want;
            }
            NUM_SG => {
                if is_to_char {
                    let s = np.sign;
                    np.out.push(s);
                    continue;
                }
                let c = np.input(np.inout_p);
                if c != b'-' && c != b'+' {
                    np.eat_non_data_chars(1);
                    continue;
                }
                np.number[0] = c;
            }
            _ => continue,
        }
        // to_number: the action consumed one input character.
        np.inout_p += 1;
    }
    if is_to_char {
        // PostgreSQL's output is a C string: anything after a NUL byte is dropped.
        let mut out = np.out;
        if let Some(k) = out.iter().position(|&c| c == 0) {
            out.truncate(k);
        }
        return Ok(out);
    }
    let mut number = np.number;
    if np.number_p > 0 && number[np.number_p - 1] == b'.' {
        number.truncate(np.number_p - 1);
    }
    np.num.post = np.read_post;
    Ok(number)
}

// --- the C printf conversions of a double ----------------------------------------------------

/// Round the digit string `c` to its first `keep` digits, half to even on the exact remainder; the
/// flag reports a carry out of the top digit ("999" → "000", carry).
fn round_digits_half_even(c: &[u8], keep: usize) -> (Vec<u8>, bool) {
    if keep >= c.len() {
        let mut v = c.to_vec();
        v.resize(keep, b'0');
        return (v, false);
    }
    let mut head = c[..keep].to_vec();
    let rest = &c[keep..];
    let up = if rest[0] > b'5' {
        true
    } else if rest[0] == b'5' {
        if rest[1..].iter().any(|&d| d != b'0') {
            true
        } else {
            keep > 0 && (head[keep - 1] - b'0') % 2 == 1
        }
    } else {
        false
    };
    if !up {
        return (head, false);
    }
    for i in (0..keep).rev() {
        if head[i] == b'9' {
            head[i] = b'0';
            continue;
        }
        head[i] += 1;
        return (head, false);
    }
    (head, true)
}

/// |v|'s exact decimal expansion as (coefficient digits without leading zeros, scale); zero is
/// ("", 0).
fn exact_digits(v: f64) -> (Vec<u8>, usize) {
    let (_, digits, scale) = Decimal::from_float64(v.abs()).canonical_digits();
    if digits == "0" {
        return (Vec::new(), 0);
    }
    (digits.into_bytes(), scale as usize)
}

fn float_sign(v: f64) -> &'static str {
    if v < 0.0 || (v == 0.0 && v.is_sign_negative()) {
        "-"
    } else {
        ""
    }
}

/// PostgreSQL's snprintf "%.*f": NaN and Infinity in PostgreSQL's spelling, the precision capped at
/// 350 with the rest zero-filled.
fn c_format_f(v: f64, prec: usize) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    let sign = float_sign(v);
    if v.is_infinite() {
        return format!("{sign}Infinity");
    }
    let p = prec.min(350);
    let (c, s) = exact_digits(v);
    // Align to p fractional digits: the integer value round(|v| * 10^p).
    let mut scaled: Vec<u8> = if c.is_empty() {
        Vec::new()
    } else if s <= p {
        let mut v = c.clone();
        v.resize(c.len() + (p - s), b'0');
        v
    } else {
        let keep = c.len() as i64 - (s - p) as i64;
        if keep < 0 {
            // Every kept digit is zero and the value is below half a unit: it rounds to zero.
            Vec::new()
        } else {
            let (mut r, carry) = round_digits_half_even(&c, keep as usize);
            if carry {
                r.insert(0, b'1');
            }
            r
        }
    };
    let lead = scaled.iter().take_while(|&&d| d == b'0').count();
    scaled.drain(..lead);
    if scaled.len() < p + 1 {
        let mut padded = vec![b'0'; p + 1 - scaled.len()];
        padded.extend_from_slice(&scaled);
        scaled = padded;
    }
    let split = scaled.len() - p;
    let mut out = String::from(sign);
    out.push_str(&String::from_utf8_lossy(&scaled[..split]));
    if p > 0 {
        out.push('.');
        out.push_str(&String::from_utf8_lossy(&scaled[split..]));
    }
    out.push_str(&"0".repeat(prec - p));
    out
}

/// PostgreSQL's snprintf "%+.*e" (`force_sign`) or "%.*e".
fn c_format_e(v: f64, prec: usize, force_sign: bool) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    let mut sign = float_sign(v);
    if sign.is_empty() && force_sign {
        sign = "+";
    }
    if v.is_infinite() {
        return format!("{sign}Infinity");
    }
    let p = prec.min(350);
    let (c, s) = exact_digits(v);
    let mut exp: i64 = 0;
    let digits: Vec<u8> = if c.is_empty() {
        vec![b'0'; p + 1]
    } else {
        exp = c.len() as i64 - 1 - s as i64;
        let (d, carry) = round_digits_half_even(&c, p + 1);
        if carry {
            exp += 1;
            let mut r = vec![b'1'];
            r.extend_from_slice(&d[..p]);
            r
        } else {
            d
        }
    };
    let mut out = String::from(sign);
    out.push(digits[0] as char);
    if p > 0 {
        out.push('.');
        out.push_str(&String::from_utf8_lossy(&digits[1..]));
    }
    out.push_str(&"0".repeat(prec - p));
    let es = if exp < 0 { "-" } else { "+" };
    out.push('e');
    out.push_str(es);
    out.push_str(&c_pad_int(exp.abs(), 2));
    out
}

/// numeric.c's numeric_out_sci: `d` as a significand rounded (half away from zero) to `scale`
/// digits, then "e" and a signed exponent of at least two digits. A significand that rounds up to
/// 10 stays 10 (PostgreSQL does not renormalize).
fn numeric_out_sci(d: &Decimal, scale: i64) -> String {
    let scale = scale.max(0) as usize;
    let (mut neg, digits, dscale) = d.canonical_digits();
    let digits = digits.into_bytes();
    let mut exp: i64 = 0;
    let sig: String = if digits == b"0" {
        neg = false;
        let mut s = String::from("0");
        if scale > 0 {
            s.push('.');
            s.push_str(&"0".repeat(scale));
        }
        s
    } else {
        exp = digits.len() as i64 - 1 - dscale as i64;
        let keep = scale + 1;
        let head: Vec<u8> = if keep >= digits.len() {
            let mut h = digits.clone();
            h.resize(keep, b'0');
            h
        } else {
            let mut hb = digits[..keep].to_vec();
            if digits[keep] >= b'5' {
                let mut carried = true;
                for i in (0..keep).rev() {
                    if !carried {
                        break;
                    }
                    if hb[i] == b'9' {
                        hb[i] = b'0';
                    } else {
                        hb[i] += 1;
                        carried = false;
                    }
                }
                if carried {
                    hb.insert(0, b'1');
                }
            }
            hb
        };
        let ip = head.len() - scale;
        let mut s = String::from_utf8_lossy(&head[..ip]).into_owned();
        if scale > 0 {
            s.push('.');
            s.push_str(&String::from_utf8_lossy(&head[ip..]));
        }
        s
    };
    let es = if exp < 0 { "-" } else { "+" };
    let out = format!("{sig}e{es}{}", c_pad_int(exp.abs(), 2));
    if neg { format!("-{out}") } else { out }
}

// --- to_char over numbers --------------------------------------------------------------------

/// NUM_TOCHAR_finish: run the processor over the laid-out number.
fn num_to_char_finish(
    nodes: &[FmtNode],
    num: &mut NumDesc,
    numstr: String,
    out_pre_spaces: i64,
    sign: u8,
) -> Result<String> {
    let out = num_processor(
        nodes,
        num,
        &[],
        numstr.into_bytes(),
        out_pre_spaces,
        sign,
        true,
    )?;
    Ok(bytes_to_string(out))
}

/// Split a rendered number into its sign and digits and size the leading padding; a number wider
/// than the template's integer digits becomes '#' fill.
fn num_layout(orgnum: &str, num: &NumDesc) -> (String, u8, i64) {
    let mut sign = b'+';
    let mut numstr = orgnum;
    if let Some(rest) = orgnum.strip_prefix('-') {
        sign = b'-';
        numstr = rest;
    }
    let pre_len = numstr.find('.').unwrap_or(numstr.len()) as i64;
    let mut out_pre_spaces = 0;
    let mut numstr = numstr.to_string();
    if pre_len < num.pre {
        out_pre_spaces = num.pre - pre_len;
    } else if pre_len > num.pre {
        numstr = hash_fill(num.pre, num.post);
    }
    (numstr, sign, out_pre_spaces)
}

/// The overflow fill: pre + post + 1 '#', with '.' at position pre.
fn hash_fill(pre: i64, post: i64) -> String {
    let mut b = vec![b'#'; (pre + post + 1) as usize];
    b[pre as usize] = b'.';
    bytes_to_string(b)
}

/// NUM_TOCHAR_prepare: an empty template gives the empty string (`None`).
fn to_char_number_prepare(tmpl: &str) -> Result<Option<(Vec<FmtNode<'_>>, NumDesc)>> {
    if tmpl.is_empty() {
        return Ok(None);
    }
    let mut num = NumDesc::default();
    let nodes = parse_format(tmpl.as_bytes(), false, &mut num)?;
    Ok(Some((nodes, num)))
}

/// to_char(i16 / i32 / i64, text): int4_to_char for the 32-bit (and smaller) types, int8_to_char
/// for i64 (whose EEEE goes through numeric, not float8).
pub(crate) fn to_char_int(v: i64, is8: bool, tmpl: &str) -> Result<String> {
    let Some((nodes, mut num)) = to_char_number_prepare(tmpl)? else {
        return Ok(String::new());
    };
    let mut v = v;
    if num.has(F_ROMAN) {
        if v > i32::MAX as i64 || v < i32::MIN as i64 {
            v = i32::MAX as i64;
        }
        return num_to_char_finish(&nodes, &mut num, int_to_roman(v), 0, 0);
    }
    if num.has(F_EEEE) {
        let s = if is8 {
            let s = numeric_out_sci(&Decimal::from_i64(v), num.post);
            if s.starts_with('-') {
                s
            } else {
                format!(" {s}")
            }
        } else {
            let s = c_format_e(v as f64, num.post as usize, true);
            match s.strip_prefix('+') {
                Some(rest) => format!(" {rest}"),
                None => s,
            }
        };
        return num_to_char_finish(&nodes, &mut num, s, 0, 0);
    }
    if num.has(F_MULTI) {
        let multi = pow10_float(num.multi);
        v = int_mul_multi(v, multi, is8)?;
        num.pre += num.multi;
    }
    let mut orgnum = v.to_string();
    let mut sign = b'+';
    if v < 0 {
        sign = b'-';
        orgnum.remove(0);
    }
    let pre_len = orgnum.len() as i64;
    let mut numstr = orgnum.clone();
    if num.post != 0 {
        numstr = format!("{orgnum}.{}", "0".repeat(num.post as usize));
    }
    let mut out_pre_spaces = 0;
    if pre_len < num.pre {
        out_pre_spaces = num.pre - pre_len;
    } else if pre_len > num.pre {
        numstr = hash_fill(num.pre, num.post);
    }
    num_to_char_finish(&nodes, &mut num, numstr, out_pre_spaces, sign)
}

/// The V multiplier on an integer: PostgreSQL's dtoi4 / dtoi8 of pow(10, n) and int4mul / int8mul,
/// each 22003 out of range.
fn int_mul_multi(v: i64, multi: f64, is8: bool) -> Result<i64> {
    let m = multi.round_ties_even();
    if !is8 {
        let over = || EngineError::new(SqlState::NumericValueOutOfRange, "integer out of range");
        if m.is_nan() || !(-2147483648.0..2147483648.0).contains(&m) {
            return Err(over());
        }
        let r = v * m as i64;
        if r > i32::MAX as i64 || r < i32::MIN as i64 {
            return Err(over());
        }
        return Ok(r);
    }
    let over = || EngineError::new(SqlState::NumericValueOutOfRange, "bigint out of range");
    if m.is_nan() || !(-9223372036854775808.0..9223372036854775808.0).contains(&m) {
        return Err(over());
    }
    v.checked_mul(m as i64).ok_or_else(over)
}

/// to_char(decimal, text) — numeric_to_char.
pub(crate) fn to_char_decimal(d: &Decimal, tmpl: &str) -> Result<String> {
    let Some((nodes, mut num)) = to_char_number_prepare(tmpl)? else {
        return Ok(String::new());
    };
    if num.has(F_ROMAN) {
        let iv = match d.to_i64_round() {
            Some(iv) if iv <= i32::MAX as i64 && iv >= i32::MIN as i64 => iv,
            _ => i32::MAX as i64,
        };
        return num_to_char_finish(&nodes, &mut num, int_to_roman(iv), 0, 0);
    }
    if num.has(F_EEEE) {
        let s = numeric_out_sci(d, num.post);
        let s = if s.starts_with('-') {
            s
        } else {
            format!(" {s}")
        };
        return num_to_char_finish(&nodes, &mut num, s, 0, 0);
    }
    let mut val = d.clone();
    if num.has(F_MULTI) {
        let digits = format!("1{}", "0".repeat(num.multi as usize));
        let p = Decimal::from_digits_scale(false, &digits, 0);
        val = d.mul(&p)?;
        num.pre += num.multi;
    }
    let x = val.round_places(num.post)?;
    let (numstr, sign, out_pre_spaces) = num_layout(&x.render(), &num);
    num_to_char_finish(&nodes, &mut num, numstr, out_pre_spaces, sign)
}

/// to_char(f32 / f64, text) — float4_to_char / float8_to_char. The digits beyond FLT_DIG (6) /
/// DBL_DIG (15) significant ones are dropped, as PostgreSQL does.
pub(crate) fn to_char_float(f: f64, is_f32: bool, tmpl: &str) -> Result<String> {
    let Some((nodes, mut num)) = to_char_number_prepare(tmpl)? else {
        return Ok(String::new());
    };
    if num.has(F_ROMAN) {
        let mut iv = i32::MAX as i64;
        let r = f.round_ties_even();
        if !r.is_nan() && (-2147483648.0..2147483648.0).contains(&r) {
            iv = r as i64;
        }
        return num_to_char_finish(&nodes, &mut num, int_to_roman(iv), 0, 0);
    }
    if num.has(F_EEEE) {
        let s = if f.is_nan() || f.is_infinite() {
            let mut b = vec![b'#'; (num.pre + num.post + 6) as usize];
            b[0] = b' ';
            b[(num.pre + 1) as usize] = b'.';
            bytes_to_string(b)
        } else {
            let s = c_format_e(f, num.post as usize, true);
            match s.strip_prefix('+') {
                Some(rest) => format!(" {rest}"),
                None => s,
            }
        };
        return num_to_char_finish(&nodes, &mut num, s, 0, 0);
    }
    let mut val = f;
    if num.has(F_MULTI) {
        let multi = pow10_float(num.multi);
        val = if is_f32 {
            ((f as f32) * (multi as f32)) as f64
        } else {
            f * multi
        };
        num.pre += num.multi;
    }
    let dig: i64 = if is_f32 { 6 } else { 15 };
    let pre_len = c_format_f(val.abs(), 0).len() as i64;
    if pre_len >= dig {
        num.post = 0;
    } else if pre_len + num.post > dig {
        num.post = dig - pre_len;
    }
    let (numstr, sign, out_pre_spaces) = num_layout(&c_format_f(val, num.post as usize), &num);
    num_to_char_finish(&nodes, &mut num, numstr, out_pre_spaces, sign)
}

// --- to_number ---------------------------------------------------------------------------------

/// to_number(text, text) → decimal (numeric_to_number): the digits the template reads, as
/// numeric(precision, scale) of the template's digit counts, then scaled by any V. An empty
/// template is NULL (`None`).
pub(crate) fn to_number_text(input: &str, tmpl: &str, m: &mut Meter) -> Result<Option<Decimal>> {
    if tmpl.is_empty() {
        return Ok(None);
    }
    let mut num = NumDesc::default();
    let nodes = parse_format(tmpl.as_bytes(), false, &mut num)?;
    let numstr = num_processor(&nodes, &mut num, input.as_bytes(), Vec::new(), 0, 0, false)?;
    let scale = num.post;
    let precision = num.pre + num.multi + scale;
    let d = parse_numeric_in(&numstr)?;
    let mut d = apply_numeric_typmod(&d, precision, scale)?;
    if num.has(F_MULTI) {
        let p = Decimal::dec_power_metered(
            &Decimal::from_i64(10),
            &Decimal::from_i64(-num.multi),
            Some(m),
        )?;
        d = d.mul(&p)?;
    }
    Ok(Some(d))
}

/// Read the processor's digits as numeric_in does: [sign] digits [. digits], surrounded by optional
/// whitespace.
fn parse_numeric_in(s: &[u8]) -> Result<Decimal> {
    let bad = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!(
                "invalid input syntax for type numeric: \"{}\"",
                String::from_utf8_lossy(s)
            ),
        )
    };
    let mut i = 0;
    while i < s.len() && is_space_c(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut digits = String::new();
    while i < s.len() && is_digit_c(s[i]) {
        digits.push(s[i] as char);
        i += 1;
    }
    let mut frac_len = 0u32;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && is_digit_c(s[i]) {
            digits.push(s[i] as char);
            frac_len += 1;
            i += 1;
        }
    }
    while i < s.len() && is_space_c(s[i]) {
        i += 1;
    }
    if i != s.len() || digits.is_empty() {
        return Err(bad());
    }
    Decimal::from_digits_scale(neg, &digits, frac_len).check_cap()
}

/// numeric.c's apply_typmod: round to `scale`, then 22003 when the integer digits exceed
/// `precision − scale`.
fn apply_numeric_typmod(d: &Decimal, precision: i64, scale: i64) -> Result<Decimal> {
    let r = d.round_to_scale(scale as u32);
    if r.is_zero() {
        return Ok(r);
    }
    let (_, digits, rs) = r.canonical_digits();
    let int_digits = digits.len() as i64 - rs as i64;
    if int_digits > precision - scale {
        return Err(EngineError::new(
            SqlState::NumericValueOutOfRange,
            "numeric field overflow",
        ));
    }
    Ok(r)
}

/// glibc's pow(10, n) for a non-negative integer n (formatting.md §6.3): the nearest double to
/// 10^n, which the decimal spelling "1e<n>" gives, except at the two exact halfway cases below
/// 10^309 (n = 23 and n = 210), where glibc takes the upper neighbor instead of the even one
/// (checked against PostgreSQL's 10::float8 ^ n for every n from 0 to 308).
fn pow10_float(n: i64) -> f64 {
    let f = format!("1e{n}").parse::<f64>().unwrap_or(f64::INFINITY);
    if n == 23 || n == 210 {
        return f64::from_bits(f.to_bits() + 1);
    }
    f
}
