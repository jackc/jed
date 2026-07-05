//! Scalar string/numeric value kernels (mirrors part of impl/go kernels.go): the string builtins
//! (pad/trim/translate/split_part, base64/hex codecs, quote_literal/quote_ident, substr/left/right) and
//! the width_bucket / numeric formatting helpers, as pure value-in/value-out free functions.

use super::*;

/// `lpad`/`rpad` over CODE POINTS (string-functions.md §3): pad `s` to `len` characters with `fill`
/// (cyclically), on the left if `left` else the right; a string longer than `len` is truncated to
/// its first `len` characters; an empty `fill` cannot pad (returns the truncated string); `len ≤ 0`
/// is empty. A `len` above `MAX_RESULT_CHARS` traps `54000`. Matches PostgreSQL's lpad/rpad.
pub(crate) fn pad_chars(s: &str, len: i64, fill: &str, left: bool) -> Result<String> {
    if len > MAX_RESULT_CHARS {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    if len <= 0 {
        return Ok(String::new());
    }
    let schars: Vec<char> = s.chars().collect();
    let slen = schars.len() as i64;
    if slen >= len {
        // longer (or equal) string truncates to its first `len` characters
        return Ok(schars[..len as usize].iter().collect());
    }
    let fchars: Vec<char> = fill.chars().collect();
    if fchars.is_empty() {
        // empty fill cannot pad — return the string unchanged (it is shorter than len)
        return Ok(s.to_string());
    }
    let need = (len - slen) as usize;
    let mut pad = String::with_capacity(need);
    for i in 0..need {
        pad.push(fchars[i % fchars.len()]);
    }
    Ok(if left {
        format!("{pad}{s}")
    } else {
        format!("{s}{pad}")
    })
}

/// `btrim`/`ltrim`/`rtrim` over CODE POINTS (string-functions.md §3): remove from the chosen end(s)
/// the longest run of characters each present in the `set` (a *set* of code points, not a substring;
/// default a single space). An empty `set` trims nothing. Matches PostgreSQL's btrim/ltrim/rtrim.
pub(crate) fn trim_chars(s: &str, set: &str, do_left: bool, do_right: bool) -> String {
    let set: std::collections::HashSet<char> = set.chars().collect();
    let chars: Vec<char> = s.chars().collect();
    let mut start = 0;
    let mut end = chars.len();
    if do_left {
        while start < end && set.contains(&chars[start]) {
            start += 1;
        }
    }
    if do_right {
        while end > start && set.contains(&chars[end - 1]) {
            end -= 1;
        }
    }
    chars[start..end].iter().collect()
}

/// `translate(s, from, to)` over CODE POINTS (string-functions.md §3): each character of `s` that
/// occurs in `from` is replaced by the character at the same position in `to`, or DELETED if `to`
/// is shorter; a character's FIRST occurrence in `from` wins. Matches PostgreSQL's translate.
pub(crate) fn translate_chars(s: &str, from: &str, to: &str) -> String {
    let tochars: Vec<char> = to.chars().collect();
    // map: from-char → Some(replacement) or None (delete). First occurrence wins (or_insert).
    let mut map: std::collections::HashMap<char, Option<char>> = std::collections::HashMap::new();
    for (i, c) in from.chars().enumerate() {
        map.entry(c).or_insert_with(|| tochars.get(i).copied());
    }
    let mut out = String::new();
    for c in s.chars() {
        match map.get(&c) {
            Some(Some(r)) => out.push(*r),
            Some(None) => {} // mapped to nothing → delete
            None => out.push(c),
        }
    }
    out
}

/// `repeat(s, n)` (string-functions.md §3): concatenate `s` `n` times; `n ≤ 0` is empty. The result's
/// byte size is bounded at `MAX_RESULT_CHARS` (PG's MaxAllocSize) — an over-large `n·|s|` traps `54000`
/// (program_limit_exceeded), the untrusted-query backstop. Matches PostgreSQL's repeat.
pub(crate) fn repeat_text(s: &str, n: i64) -> Result<String> {
    if n <= 0 {
        return Ok(String::new());
    }
    let too_large = (s.len() as i64)
        .checked_mul(n)
        .is_none_or(|total| total > MAX_RESULT_CHARS);
    if too_large {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    Ok(s.repeat(n as usize))
}

/// `split_part(s, delim, n)` (string-functions.md §3): split `s` on the substring `delim` and return
/// the n-th field (1-based; a negative n counts from the end). Out of range → `''`; `n = 0` traps
/// `22023`. An EMPTY `delim` treats the whole string as one field (str::split would otherwise split
/// between every character — a cross-core trap). Matches PostgreSQL's split_part.
pub(crate) fn split_part(s: &str, delim: &str, n: i64) -> Result<String> {
    if n == 0 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "field position must not be zero",
        ));
    }
    let fields: Vec<&str> = if delim.is_empty() {
        vec![s]
    } else {
        s.split(delim).collect()
    };
    let len = fields.len() as i64;
    let idx = if n > 0 { n - 1 } else { len + n };
    if idx < 0 || idx >= len {
        return Ok(String::new());
    }
    Ok(fields[idx as usize].to_string())
}

/// `chr(n)` (string-functions.md §3): the one-character string for the Unicode code point `n`.
/// PostgreSQL's error split: a negative `n` traps `22023`; `0`, a value above `U+10FFFF`, and a
/// UTF-16 surrogate (`U+D800..U+DFFF`, rejected by `char::from_u32`) trap `54000`.
pub(crate) fn chr_text(n: i64) -> Result<String> {
    if n < 0 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "character number must be positive",
        ));
    }
    if n == 0 {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "null character not permitted",
        ));
    }
    if n > 0x10FFFF {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            format!("requested character too large for encoding: {n}"),
        ));
    }
    match char::from_u32(n as u32) {
        Some(c) => Ok(c.to_string()),
        // a surrogate code point has no scalar value
        None => Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            format!("requested character not valid for encoding: {n}"),
        )),
    }
}

/// The standard RFC 4648 base64 alphabet (string-functions.md §3, shared by encode/decode).
pub(crate) const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `encode(bytes, format)` (string-functions.md §3): render binary as text. `hex` = two lowercase
/// hex digits per byte; `base64` = RFC 4648, wrapped at 76 chars with `\n` (PostgreSQL's style);
/// `escape` = printable bytes verbatim, `0x00` → `\000`, backslash doubled, high-bit bytes → `\nnn`
/// octal. An unrecognized format traps `22023`.
pub(crate) fn encode_bytea(bytes: &[u8], format: &str) -> Result<String> {
    match format {
        "hex" => Ok(bytes.iter().map(|b| format!("{b:02x}")).collect()),
        "escape" => {
            let mut out = String::with_capacity(bytes.len());
            for &b in bytes {
                match b {
                    0x00 => out.push_str("\\000"),
                    0x5c => out.push_str("\\\\"),
                    0x80..=0xff => out.push_str(&format!("\\{b:03o}")),
                    _ => out.push(b as char), // 0x01..0x7f except backslash — a single ASCII byte
                }
            }
            Ok(out)
        }
        "base64" => Ok(base64_encode_wrapped(bytes)),
        _ => Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("unrecognized encoding: \"{format}\""),
        )),
    }
}

/// RFC 4648 base64 of `bytes`, wrapped at 76 characters with `\n` between chunks (no trailing
/// newline) — PostgreSQL's `encode(…, 'base64')` layout.
pub(crate) fn base64_encode_wrapped(bytes: &[u8]) -> String {
    let mut b64: Vec<u8> = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        b64.push(BASE64_ALPHABET[((n >> 18) & 63) as usize]);
        b64.push(BASE64_ALPHABET[((n >> 12) & 63) as usize]);
        b64.push(if chunk.len() > 1 {
            BASE64_ALPHABET[((n >> 6) & 63) as usize]
        } else {
            b'='
        });
        b64.push(if chunk.len() > 2 {
            BASE64_ALPHABET[(n & 63) as usize]
        } else {
            b'='
        });
    }
    let mut out = String::with_capacity(b64.len() + b64.len() / 76);
    for (i, &c) in b64.iter().enumerate() {
        if i > 0 && i % 76 == 0 {
            out.push('\n');
        }
        out.push(c as char);
    }
    out
}

/// `quote_literal(s)` (string-functions.md §3): wrap `s` as a SQL string literal — single-quoted,
/// each internal `'` doubled; if `s` contains a backslash, each `\` is doubled and the literal is
/// `E`-prefixed (matching PostgreSQL). Shared by `quote_literal` and `quote_nullable`.
pub(crate) fn quote_literal_text(s: &str) -> String {
    let has_backslash = s.contains('\\');
    let mut inner = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\'' => inner.push_str("''"),
            '\\' => inner.push_str("\\\\"),
            _ => inner.push(c),
        }
    }
    if has_backslash {
        format!("E'{inner}'")
    } else {
        format!("'{inner}'")
    }
}

/// `quote_ident(s)` (string-functions.md §3): wrap `s` as a SQL identifier — returned unchanged if it
/// is already a safe unquoted identifier (`^[a-z_][a-z0-9_]*$`), else double-quoted with each internal
/// `"` doubled. jed quotes by the LEXICAL pattern only — no reserved-keyword quoting (jed has no
/// enumerated keyword set), a documented divergence from PostgreSQL.
pub(crate) fn quote_ident_text(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                b == b'_' || b.is_ascii_lowercase()
            } else {
                b == b'_' || b.is_ascii_lowercase() || b.is_ascii_digit()
            }
        });
    if safe {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push_str("\"\"");
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

/// `decode(s, format)` (string-functions.md §3): the inverse of `encode`. `hex` and `base64` ignore
/// whitespace; a malformed hex/base64 string traps `22023`; a malformed `escape` sequence traps
/// `22P02` (PostgreSQL's split). An unrecognized format traps `22023`.
pub(crate) fn decode_text(s: &str, format: &str) -> Result<Vec<u8>> {
    match format {
        "hex" => decode_hex(s.as_bytes()),
        "base64" => decode_base64(s.as_bytes()),
        "escape" => decode_escape(s.as_bytes()),
        _ => Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("unrecognized encoding: \"{format}\""),
        )),
    }
}

pub(crate) fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decode `hex`: pairs of hex digits (case-insensitive); whitespace is ignored; a non-hex byte or an
/// odd digit count traps `22023`.
pub(crate) fn decode_hex(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut nibbles: Vec<u8> = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_whitespace() {
            continue;
        }
        nibbles.push(hex_nibble(b).ok_or_else(|| {
            EngineError::new(SqlState::InvalidParameterValue, "invalid hexadecimal digit")
        })?);
    }
    if nibbles.len() % 2 != 0 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "invalid hexadecimal data: odd number of digits",
        ));
    }
    Ok(nibbles.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

/// Decode `base64` (RFC 4648); whitespace is ignored; an out-of-alphabet byte (or data after the
/// `=` padding) traps `22023`. Bit-accumulation: each 6-bit symbol feeds a buffer that emits a byte
/// per full 8 bits (the trailing <8 bits implied by padding are discarded).
pub(crate) fn decode_base64(bytes: &[u8]) -> Result<Vec<u8>> {
    let bad = || {
        EngineError::new(
            SqlState::InvalidParameterValue,
            "invalid base64 end sequence",
        )
    };
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits = 0;
    let mut padded = false;
    for &b in bytes {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            padded = true;
            continue;
        }
        if padded {
            return Err(bad()); // a data symbol after padding
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(bad()),
        };
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Ok(out)
}

/// Decode `escape` (operating on the input's UTF-8 bytes): `\\` → backslash, `\nnn` (exactly 3 octal
/// digits, value ≤ 255) → that byte, any other byte → itself. A lone/short backslash or an octal >
/// 255 traps `22P02`.
pub(crate) fn decode_escape(bytes: &[u8]) -> Result<Vec<u8>> {
    let bad = || {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            "invalid input syntax for type bytea",
        )
    };
    let oct = |c: u8| (b'0'..=b'7').contains(&c).then_some(c - b'0');
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            out.push(b'\\');
            i += 2;
        } else if i + 3 < bytes.len() {
            match (oct(bytes[i + 1]), oct(bytes[i + 2]), oct(bytes[i + 3])) {
                (Some(a), Some(b), Some(c)) => {
                    let v = (a as u16) * 64 + (b as u16) * 8 + c as u16;
                    if v > 255 {
                        return Err(bad());
                    }
                    out.push(v as u8);
                    i += 4;
                }
                _ => return Err(bad()),
            }
        } else {
            return Err(bad());
        }
    }
    Ok(out)
}

/// `initcap(s)` (string-functions.md §3): uppercase the first character of each word and lowercase
/// the rest, where a *word* is a maximal run of ASCII alphanumerics. jed classifies word boundaries
/// by ASCII alphanumerics and folds ASCII case only — deterministic and cross-core-identical (full
/// Unicode word classification would risk the cross-core Unicode-version trap, §3). PostgreSQL agrees
/// for ASCII input; a non-ASCII letter is treated as a word boundary (a documented divergence).
pub(crate) fn initcap_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut word_start = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(if word_start {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            });
            word_start = false;
        } else {
            out.push(c);
            word_start = true;
        }
    }
    out
}

/// `substr(s, start[, count])` over CODE POINTS (string-functions.md §3): 1-based; the window
/// `[start, start+count)` (or `[start, ∞)` for the 2-arg form) intersected with `[1, n]`. A start
/// ≤ 0 / past the end clips; a NEGATIVE count traps 22011. Matches PostgreSQL's text `substr`.
pub(crate) fn substr_chars(s: &str, start: i64, count: Option<i64>) -> Result<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let to = match count {
        Some(c) => {
            if c < 0 {
                return Err(EngineError::new(
                    SqlState::SubstringError,
                    "negative substring length not allowed",
                ));
            }
            start.saturating_add(c).min(n + 1)
        }
        None => n + 1,
    };
    let from = start.max(1);
    if to <= from {
        return Ok(String::new());
    }
    Ok(chars[(from - 1) as usize..(to - 1) as usize]
        .iter()
        .collect())
}

/// `left(s, n)` over CODE POINTS (string-functions.md §3): the first `n` characters; a negative
/// `n` returns all but the last `|n|`. Matches PostgreSQL's `left`.
pub(crate) fn left_chars(s: &str, n: i64) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let end = if n < 0 {
        len.saturating_add(n).max(0)
    } else {
        n.min(len)
    };
    chars[..end as usize].iter().collect()
}

/// `right(s, n)` over CODE POINTS (string-functions.md §3): the last `n` characters; a negative
/// `n` returns all but the first `|n|`. Matches PostgreSQL's `right`.
pub(crate) fn right_chars(s: &str, n: i64) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let start = if n < 0 {
        // skip the first |n|; checked_neg guards n == i64::MIN (skip everything).
        n.checked_neg().unwrap_or(i64::MAX).min(len)
    } else {
        (len - n).max(0)
    };
    chars[start as usize..].iter().collect()
}

/// The `decimal_work` W of an arithmetic node — which group-count formula applies per op
/// (spec/design/cost.md §3 "decimal_work"). The evaluator charges W − 1 before the op runs.
pub(crate) fn decimal_arith_work(op: ArithOp, a: &Decimal, b: &Decimal) -> u64 {
    match op {
        ArithOp::Add | ArithOp::Sub => decimal::work_linear(a, b),
        ArithOp::Mul => decimal::work_mul(a, b),
        ArithOp::Div => decimal::work_div(a, b),
        ArithOp::Mod => decimal::work_mod(a, b),
    }
}

/// The `decimal_work` W of a comparison over a decimal(-promotable) pair — the aligned
/// linear formula after `int → decimal` promotion; 1 (no charge) for any other pair,
/// including a NULL side, where no decimal compare runs (cost.md §3 "decimal_work").
pub(crate) fn decimal_cmp_work(a: &Value, b: &Value) -> u64 {
    match (a, b) {
        (Value::Decimal(x), Value::Decimal(y)) => decimal::work_linear(x, y),
        (Value::Decimal(x), Value::Int(y)) => decimal::work_linear(x, &Decimal::from_i64(*y)),
        (Value::Int(x), Value::Decimal(y)) => decimal::work_linear(&Decimal::from_i64(*x), y),
        _ => 1,
    }
}

/// Per-operator cost-base overrides, keyed by operator name — the `OPERATORS` rows whose catalog
/// `cost` is non-default (functions.md §8). Empty while every built-in uses the uniform
/// `operator_eval`; authoring a `cost` in catalog.toml populates it (a pure data change, no code).
/// The `cost == 0` sentinel means "use operator_eval". Built once from the generated table.
pub(crate) static OP_COST_OVERRIDES: LazyLock<HashMap<&'static str, i64>> = LazyLock::new(|| {
    OPERATORS
        .iter()
        .filter(|o| o.cost != 0)
        .map(|o| (o.name, o.cost))
        .collect()
});

/// The cost an operator's evaluation charges: its catalog `cost` base if authored, else the uniform
/// `operator_eval` (cost.md §3). The `is_empty` fast path keeps the common all-default case a single
/// check, so no per-node name hashing happens until a weight is actually tuned.
pub(crate) fn operator_cost(name: &str) -> i64 {
    if OP_COST_OVERRIDES.is_empty() {
        return COSTS.operator_eval;
    }
    OP_COST_OVERRIDES
        .get(name)
        .copied()
        .unwrap_or(COSTS.operator_eval)
}

/// The `varlen_compare` W of a comparison over a variable-length scalar pair — the SHORTER
/// operand's length (code points for `text`, bytes for `bytea`), clamped to ≥ 1. A byte /
/// code-point comparison stops at the first differing position or the end of the shorter
/// operand, so `min` is a true upper bound on the work (cost.md §3 "varlen_compare"). Any
/// other pair — including a NULL side or a non-varlen type — returns 1 (no charge).
pub(crate) fn varlen_compare_work(a: &Value, b: &Value) -> i64 {
    let n = match (a, b) {
        (Value::Text(x), Value::Text(y)) => x.chars().count().min(y.chars().count()),
        (Value::Bytea(x), Value::Bytea(y)) => x.len().min(y.len()),
        _ => return 1,
    };
    (n as i64).max(1)
}

/// Evaluate decimal arithmetic with PG's result-scale rules (spec/design/decimal.md §4),
/// trapping 22003 at the cap and 22012 on a zero divisor/modulus.
pub(crate) fn eval_decimal_arith(op: ArithOp, a: Decimal, b: Decimal) -> Result<Value> {
    let r = match op {
        ArithOp::Add => a.add(&b)?,
        ArithOp::Sub => a.sub(&b)?,
        ArithOp::Mul => a.mul(&b)?,
        ArithOp::Div => a.div(&b)?,
        ArithOp::Mod => a.rem(&b)?,
    };
    Ok(Value::Decimal(r))
}
