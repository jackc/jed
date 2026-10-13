//! Scalar string/numeric value kernels (mirrors part of impl/go kernels.go): the string builtins
//! (pad/trim/translate/split_part, base64/hex codecs, quote_literal/quote_ident, substr/left/right) and
//! the width_bucket / numeric formatting helpers, as pure value-in/value-out free functions.

use super::*;

/// `lpad`/`rpad` over CODE POINTS (string-functions.md §3): pad `s` to `len` characters with `fill`
/// (cyclically), on the left if `left` else the right; a string longer than `len` is truncated to
/// its first `len` characters; an empty `fill` cannot pad (returns the truncated string); `len ≤ 0`
/// is empty. A `len` above `MAX_RESULT_CHARS` traps `54000`. Matches PostgreSQL's lpad/rpad.
fn text_prefix_bytes(s: &str, chars: i64) -> usize {
    s.char_indices()
        .nth(chars as usize)
        .map_or(s.len(), |(i, _)| i)
}

pub(crate) fn pad_chars(
    s: &str,
    len: i64,
    fill: &str,
    left: bool,
    meter: &mut Meter,
) -> Result<String> {
    if len > MAX_RESULT_CHARS {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    if len <= 0 {
        return Ok(String::new());
    }
    meter.charge(COSTS.scalar_byte * (s.len() + fill.len()) as i64);
    meter.guard()?;
    let prefix = text_prefix_bytes(s, len);
    let need = len - s[..prefix].chars().count() as i64;
    let (mut cycles, mut tail) = (0i64, 0usize);
    let mut size = prefix as i64;
    if need > 0 && !fill.is_empty() {
        let flen = fill.chars().count() as i64;
        cycles = need / flen;
        tail = text_prefix_bytes(fill, need % flen);
        if cycles > (MAX_RESULT_CHARS - size - tail as i64) / fill.len() as i64 {
            return Err(EngineError::new(
                SqlState::ProgramLimitExceeded,
                "requested length too large",
            ));
        }
        size += cycles * fill.len() as i64 + tail as i64;
    }
    if size > MAX_RESULT_CHARS {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    meter.charge(COSTS.scalar_byte * size);
    meter.guard()?;
    meter.reserve_scalar(size)?;
    let mut out = String::with_capacity(size as usize);
    if !left {
        out.push_str(&s[..prefix]);
    }
    for _ in 0..cycles {
        out.push_str(fill);
    }
    out.push_str(&fill[..tail]);
    if left {
        out.push_str(&s[..prefix]);
    }
    Ok(out)
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
pub(crate) fn repeat_text(s: &str, n: i64, meter: &mut Meter) -> Result<String> {
    if n <= 0 || s.is_empty() {
        return Ok(String::new());
    }
    if n > MAX_RESULT_CHARS / s.len() as i64 {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    meter.charge(COSTS.scalar_byte * s.len() as i64);
    meter.guard()?;
    let size = n * s.len() as i64;
    meter.charge(COSTS.scalar_byte * size);
    meter.guard()?;
    meter.reserve_scalar(size)?;
    Ok(s.repeat(n as usize))
}

/// Charge a growth kernel's constructed result (spec/design/cost.md §8.1): `scalar_byte ×
/// payload(result)`, guarded before the value is returned. A NULL result charges nothing.
pub(crate) fn charge_output(meter: &mut Meter, out: &Value) -> Result<()> {
    if matches!(out, Value::Null) {
        return Ok(());
    }
    meter.charge(COSTS.scalar_byte * crate::memsize::payload_bytes(out));
    meter.guard()
}

/// `replace(s, from, to)` (string-functions.md §3): replace every non-overlapping occurrence of
/// `from`; an empty `from` matches nothing (PostgreSQL; Rust's `str::replace` would instead splice
/// `to` at every boundary). An amplifier (cost.md §8.1): the input scan is charged, then the exact
/// UTF-8 result size is computed from the match count, capped at `MAX_RESULT_CHARS` (`54000`),
/// charged, and reserved before the result is built.
pub(crate) fn replace_text(s: &str, from: &str, to: &str, meter: &mut Meter) -> Result<String> {
    meter.charge(COSTS.scalar_byte * s.len() as i64);
    meter.guard()?;
    let count = if from.is_empty() {
        0
    } else {
        s.matches(from).count() as i64
    };
    let size = s.len() as i64 + count * (to.len() as i64 - from.len() as i64);
    if size > MAX_RESULT_CHARS {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    meter.charge(COSTS.scalar_byte * size);
    meter.guard()?;
    meter.reserve_scalar(size)?;
    Ok(if count == 0 {
        s.to_string()
    } else {
        s.replace(from, to)
    })
}

/// `regexp_replace` (regex.md §8): run the whole search, then size the result exactly, cap it at
/// `MAX_RESULT_CHARS` (`54000`), charge and reserve it, and splice it from the recorded matches. An
/// amplifier (cost.md §8.1); the search's `regex_step`s are its input charge.
pub(crate) fn regexp_replace_text(
    prog: &crate::regex::Program,
    match_chars: &[char],
    orig_chars: &[char],
    repl: &[char],
    global: bool,
    meter: &mut Meter,
) -> Result<String> {
    let found = prog.replace_matches(match_chars, global, meter)?;
    let size = crate::regex::replace_size(orig_chars, repl, &found);
    if size > MAX_RESULT_CHARS {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    meter.charge(COSTS.scalar_byte * size);
    meter.guard()?;
    meter.reserve_scalar(size)?;
    Ok(crate::regex::replace_build(orig_chars, repl, &found, size))
}

/// `regexp_match` / `regexp_matches` (regex.md §8/§8c): one `text[]` capture array for the first
/// match, or for every non-overlapping match when `global`; no match gives none. An amplifier
/// (cost.md §8.1): nested groups capture the same text once each, so the pattern multiplies the
/// subject. The search's `regex_step`s are its input charge; the arrays' exact payload is then
/// computed from the capture spans, charged, and reserved before any is built.
pub(crate) fn regexp_capture_arrays(
    prog: &crate::regex::Program,
    match_chars: &[char],
    orig_chars: &[char],
    global: bool,
    meter: &mut Meter,
) -> Result<Vec<Value>> {
    let found = prog.replace_matches(match_chars, global, meter)?;
    if found.is_empty() {
        return Ok(Vec::new());
    }
    let utf8_at = crate::regex::utf8_offsets(orig_chars);
    let size: i64 = found
        .iter()
        .map(|saves| prog.capture_array_size(saves, &utf8_at))
        .sum();
    meter.charge(COSTS.scalar_byte * size);
    meter.guard()?;
    meter.reserve_scalar(size)?;
    let arrays: Vec<Value> = found
        .iter()
        .map(|saves| {
            let elems = prog
                .capture_array(saves, orig_chars)
                .into_iter()
                .map(|g| g.map_or(Value::Null, Value::Text))
                .collect();
            Value::Array(ArrayVal::one_dim(elems))
        })
        .collect();
    debug_assert_eq!(
        arrays
            .iter()
            .map(crate::memsize::payload_bytes)
            .sum::<i64>(),
        size,
        "capture_array_size disagrees with the built arrays"
    );
    Ok(arrays)
}

/// `jsonb_pretty(j)` (json-sql-functions.md §2): an amplifier, because indentation grows with depth
/// times node count (cost.md §8.1). The input payload is charged, then the exact rendering size is
/// computed, capped at `MAX_RESULT_CHARS` (`54000`), charged, and reserved before rendering.
pub(crate) fn jsonb_pretty_text(arg: &Value, meter: &mut Meter) -> Result<String> {
    meter.charge(COSTS.scalar_byte * crate::memsize::payload_bytes(arg));
    meter.guard()?;
    let node = json_arg_node(arg)?;
    let size = json::pretty_size(&node);
    if size > MAX_RESULT_CHARS {
        return Err(EngineError::new(
            SqlState::ProgramLimitExceeded,
            "requested length too large",
        ));
    }
    meter.charge(COSTS.scalar_byte * size);
    meter.guard()?;
    meter.reserve_scalar(size)?;
    let out = json::pretty(&node);
    debug_assert_eq!(out.len() as i64, size, "pretty_size disagrees with pretty");
    Ok(out)
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

/// `like_escape(pattern, escape)` (string-functions.md §3): rewrite a LIKE pattern written with the
/// escape character `escape` into the default-`\` form the matcher reads (PostgreSQL's
/// `MB_do_like_escape`), by code point. `''` disables escaping (each `\` doubled); `\` returns the
/// pattern unchanged; any other single code point becomes `\`, with a `\` after it a literal
/// backslash. An escape of two or more code points traps 22025 (invalid escape string).
pub(crate) fn like_escape_text(pattern: &str, escape: &str) -> Result<String> {
    let mut esc_chars = escape.chars();
    let esc = match (esc_chars.next(), esc_chars.next()) {
        (None, _) => None,
        (Some(c), None) => Some(c),
        (Some(_), Some(_)) => {
            return Err(EngineError::new(
                SqlState::InvalidEscapeSequence,
                "invalid escape string",
            ));
        }
    };
    let mut out = String::with_capacity(pattern.len() + 2);
    match esc {
        None => {
            for c in pattern.chars() {
                if c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
        }
        Some('\\') => out.push_str(pattern),
        Some(e) => {
            let mut after_escape = false;
            for c in pattern.chars() {
                if c == e && !after_escape {
                    out.push('\\');
                    after_escape = true;
                } else if c == '\\' {
                    out.push('\\');
                    if !after_escape {
                        out.push('\\');
                    }
                    after_escape = false;
                } else {
                    out.push(c);
                    after_escape = false;
                }
            }
        }
    }
    Ok(out)
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

/// `decode(s, format)` (string-functions.md §3): the inverse of `encode`. `hex` and `base64` skip a
/// space, tab, newline, or carriage return (`hex` only between digit pairs); a malformed hex/base64
/// string traps `22023`; a malformed `escape` sequence traps `22P02` (PostgreSQL's split). An
/// unrecognized format traps `22023`.
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

/// Decode `hex`: PostgreSQL's `hex_decode_safe` (`value::decode_hex_pairs`, shared with bytea hex
/// input) — hex digit pairs, with a space, tab, newline, or carriage return skipped between pairs;
/// an invalid digit (whitespace inside a pair included) or an odd digit count traps `22023`.
pub(crate) fn decode_hex(bytes: &[u8]) -> Result<Vec<u8>> {
    crate::value::decode_hex_pairs(bytes)
        .map_err(|msg| EngineError::new(SqlState::InvalidParameterValue, msg))
}

/// Decode `base64` (RFC 4648); a space, tab, newline, or carriage return is ignored (PostgreSQL's
/// `pg_base64_decode` set); an out-of-alphabet byte (or data after the `=` padding) traps `22023`.
/// Bit-accumulation: each 6-bit symbol feeds a buffer that emits a byte per full 8 bits (the
/// trailing <8 bits implied by padding are discarded).
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
        if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
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

/// `substr(b, start[, count])` over BYTES (string-functions.md §4): `substr_chars`'s window rule
/// applied to a bytea — `[start, start+count)` intersected with `[1, n]`, a negative count 22011.
pub(crate) fn substr_bytes(b: &[u8], start: i64, count: Option<i64>) -> Result<Vec<u8>> {
    let n = b.len() as i64;
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
        return Ok(Vec::new());
    }
    Ok(b[(from - 1) as usize..(to - 1) as usize].to_vec())
}

/// `get_byte(b, n)` (string-functions.md §4): the 0-based `n`-th byte as an unsigned integer. An
/// index outside `[0, len-1]` is 2202E with PostgreSQL's exact message (`0..-1` when empty).
pub(crate) fn get_byte(b: &[u8], n: i64) -> Result<i64> {
    match usize::try_from(n).ok().and_then(|i| b.get(i)) {
        Some(byte) => Ok(i64::from(*byte)),
        None => Err(EngineError::new(
            SqlState::ArraySubscriptError,
            format!("index {n} out of valid range, 0..{}", b.len() as i64 - 1),
        )),
    }
}

/// The 2202E index error the bytea byte/bit accessors share: PostgreSQL's exact message,
/// `index n out of valid range, 0..max` (`0..-1` for an empty value).
fn bytea_index_error(n: i64, max: i64) -> EngineError {
    EngineError::new(
        SqlState::ArraySubscriptError,
        format!("index {n} out of valid range, 0..{max}"),
    )
}

/// `set_byte(b, n, v)` (string-functions.md §4): a copy of `b` with the 0-based `n`-th byte set to
/// the low 8 bits of `v` (PostgreSQL stores the int as an unsigned char: 256 → 0, -1 → 255). An
/// index outside `[0, len-1]` is get_byte's 2202E.
pub(crate) fn set_byte(b: &[u8], n: i64, v: i64) -> Result<Vec<u8>> {
    let i = match usize::try_from(n) {
        Ok(i) if i < b.len() => i,
        _ => return Err(bytea_index_error(n, b.len() as i64 - 1)),
    };
    let mut out = b.to_vec();
    out[i] = v as u8;
    Ok(out)
}

/// The `(byte, bit)` address of bit `n` of a bytea of `len` bytes for get_bit / set_bit
/// (string-functions.md §4): bit `n % 8` (least-significant first) of byte `n / 8`. An index outside
/// `[0, len*8-1]` is 2202E with PostgreSQL's message.
fn bit_address(len: usize, n: i64) -> Result<(usize, u32)> {
    let bits = len as i64 * 8;
    if n < 0 || n >= bits {
        return Err(bytea_index_error(n, bits - 1));
    }
    Ok(((n / 8) as usize, (n % 8) as u32))
}

/// `get_bit(b, n)` (string-functions.md §4): the 0-based `n`-th bit as 0 or 1.
pub(crate) fn get_bit(b: &[u8], n: i64) -> Result<i64> {
    let (byte, bit) = bit_address(b.len(), n)?;
    Ok(i64::from((b[byte] >> bit) & 1))
}

/// `set_bit(b, n, v)` (string-functions.md §4): a copy of `b` with the `n`-th bit set to `v`. The
/// index is checked first (2202E), then `v` must be 0 or 1 (22023), PostgreSQL's order.
pub(crate) fn set_bit(b: &[u8], n: i64, v: i64) -> Result<Vec<u8>> {
    let (byte, bit) = bit_address(b.len(), n)?;
    if v != 0 && v != 1 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "new bit must be 0 or 1",
        ));
    }
    let mut out = b.to_vec();
    if v == 0 {
        out[byte] &= !(1u8 << bit);
    } else {
        out[byte] |= 1u8 << bit;
    }
    Ok(out)
}

/// The tail start `start + count` of `overlay(s PLACING r FROM start FOR count)` (string-functions.md
/// §3/§4), checked as PostgreSQL does: a start ≤ 0 is 22011 ("negative substring length not
/// allowed", the standard's definition through substring), and a `start + count` outside the int4
/// range is 22003. The result is `substr(s, 1, start-1) || r || substr(s, start+count)`.
pub(crate) fn overlay_tail_start(start: i64, count: i64) -> Result<i64> {
    if start <= 0 {
        return Err(EngineError::new(
            SqlState::SubstringError,
            "negative substring length not allowed",
        ));
    }
    match start.checked_add(count) {
        Some(t) if ScalarType::Int32.in_range(t) => Ok(t),
        _ => Err(overflow(ScalarType::Int32)),
    }
}

/// `overlay` over CODE POINTS (string-functions.md §3): `s` with `count` characters from the 1-based
/// `start` replaced by `r` (see [`overlay_tail_start`]).
pub(crate) fn overlay_chars(s: &str, r: &str, start: i64, count: i64) -> Result<String> {
    let tail = overlay_tail_start(start, count)?;
    let mut out = substr_chars(s, 1, Some(start - 1))?;
    out.push_str(r);
    out.push_str(&substr_chars(s, tail, None)?);
    Ok(out)
}

/// `overlay` over BYTES (string-functions.md §4): [`overlay_chars`]'s splice over a bytea.
pub(crate) fn overlay_bytes(b: &[u8], r: &[u8], start: i64, count: i64) -> Result<Vec<u8>> {
    let tail = overlay_tail_start(start, count)?;
    let mut out = substr_bytes(b, 1, Some(start - 1))?;
    out.extend_from_slice(r);
    out.extend_from_slice(&substr_bytes(b, tail, None)?);
    Ok(out)
}

/// `btrim`/`ltrim`/`rtrim(bytea, bytes)` (string-functions.md §4): remove from the chosen end(s) the
/// longest run of bytes each present in the `set` bytes. An empty `set` trims nothing.
pub(crate) fn trim_bytes(b: &[u8], set: &[u8], do_left: bool, do_right: bool) -> Vec<u8> {
    let mut start = 0;
    let mut end = b.len();
    if do_left {
        while start < end && set.contains(&b[start]) {
            start += 1;
        }
    }
    if do_right {
        while end > start && set.contains(&b[end - 1]) {
            end -= 1;
        }
    }
    b[start..end].to_vec()
}

/// `position(bytea, sub)` (string-functions.md §4): the 1-based byte position of the first
/// occurrence of `sub` in `b`, else 0; an empty `sub` is 1. PostgreSQL's byteapos.
pub(crate) fn position_bytes(b: &[u8], sub: &[u8]) -> i64 {
    if sub.is_empty() {
        return 1;
    }
    b.windows(sub.len())
        .position(|w| w == sub)
        .map_or(0, |i| i as i64 + 1)
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
