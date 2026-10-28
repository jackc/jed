//! Hand-written lexer for the step-1 SQL surface (CLAUDE.md §5: parsers are
//! per-language, not codegen'd). Produces a flat token vector.

use crate::decimal::{EXP_LIMIT, decimal_from_parts};
use crate::error::{EngineError, Result, SqlState};
use crate::token::Token;

/// Is `c` a PostgreSQL identifier-start byte (scan.l `ident_start`: `[A-Za-z\x80-\xff_]`)? Used
/// only to detect an identifier glued to a numeric literal (trailing junk, grammar.md §4).
fn ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

/// The length of the PostgreSQL identifier (`ident_start ident_cont*`, where `ident_cont` adds
/// digits and `$`) starting at `bytes[p]`; 0 when none starts there.
fn ident_len(bytes: &[u8], p: usize) -> usize {
    if p >= bytes.len() || !ident_start(bytes[p]) {
        return 0;
    }
    let mut q = p + 1;
    while q < bytes.len()
        && (ident_start(bytes[q]) || bytes[q].is_ascii_digit() || bytes[q] == b'$')
    {
        q += 1;
    }
    q - p
}

/// The end of the radix-`base` digit run `(_? digit)*` starting at `bytes[p]` — a `_` counts only
/// when a digit follows it (PostgreSQL's `hexinteger` tail; a `decinteger` caller first checks its
/// leading digit).
fn digit_run_end(bytes: &[u8], mut p: usize, base: u32) -> usize {
    let is_digit = |q: usize| q < bytes.len() && char::from(bytes[q]).is_digit(base);
    loop {
        if is_digit(p) {
            p += 1;
        } else if p < bytes.len() && bytes[p] == b'_' && is_digit(p + 1) {
            p += 2;
        } else {
            return p;
        }
    }
}

/// `text` without its `_` digit separators.
fn strip_underscores(text: &str) -> String {
    text.chars().filter(|&c| c != '_').collect()
}

/// An integer literal's unsigned magnitude from its radix-`base` digits (`_` separators allowed).
/// It must be `<= 2^63` so that `-(2^63)` = `i64::MIN` is reachable; anything larger cannot be
/// represented (42601). `i64` cannot hold `2^63`, so it is carried unsigned for the parser.
fn integer_magnitude(written: &str, digits: &str, base: u32) -> Result<u64> {
    let out_of_range = || syntax(format!("integer literal out of range: {written}"));
    let mut n: u64 = 0;
    for c in digits.chars().filter(|&c| c != '_') {
        let d = u64::from(c.to_digit(base).expect("a scanned digit"));
        n = n
            .checked_mul(u64::from(base))
            .and_then(|n| n.checked_add(d))
            .filter(|&n| n <= 1u64 << 63)
            .ok_or_else(out_of_range)?;
    }
    Ok(n)
}

/// Scan the numeric literal starting at `sql[start]` (a digit, or a `.` followed by a digit) with
/// PostgreSQL's scan.l rules (grammar.md §4): a decimal integer `digit (_? digit)*`; a radix
/// integer `0x`/`0o`/`0b` `(_? digit)+` (a bare `0x` / `0x_` is "invalid hexadecimal integer"); a
/// decimal with a `.` and/or an exponent `[eE][+-]?decinteger`. Any identifier character glued to
/// the end (`123abc`, `1_`, `0x1g`, `1e`), or an exponent sign with no digit (`1e+`), is
/// "trailing junk after numeric literal". `1..` is the integer `1` followed by the `..` (rescanned).
/// Returns the token and the byte offset just past it. All errors are 42601.
fn scan_number(sql: &str, start: usize) -> Result<(Token, usize)> {
    let bytes = sql.as_bytes();
    let at = |k: usize| bytes.get(k).copied().unwrap_or(0);
    let junk = |end: usize| {
        syntax(format!(
            "trailing junk after numeric literal at or near \"{}\"",
            &sql[start..end]
        ))
    };
    // A radix integer: `0x…` / `0o…` / `0b…`.
    if at(start) == b'0' {
        let (base, name) = match at(start + 1) {
            b'x' | b'X' => (16, "hexadecimal"),
            b'o' | b'O' => (8, "octal"),
            b'b' | b'B' => (2, "binary"),
            _ => (0, ""),
        };
        if base != 0 {
            let end = digit_run_end(bytes, start + 2, base);
            // `0` followed by an identifier is the longest junk reading (scan.l `integer_junk`).
            let junk_end = start + 1 + ident_len(bytes, start + 1);
            if end > start + 2 {
                if junk_end > end {
                    return Err(junk(junk_end));
                }
                let n = integer_magnitude(&sql[start..end], &sql[start + 2..end], base)?;
                return Ok((Token::Int(n), end));
            }
            let fail_end = start + 2 + usize::from(at(start + 2) == b'_');
            if junk_end > fail_end {
                return Err(junk(junk_end));
            }
            return Err(syntax(format!(
                "invalid {name} integer at or near \"{}\"",
                &sql[start..fail_end]
            )));
        }
    }
    // A decimal integer part (empty for a leading-dot decimal `.5`).
    let int_end = if at(start).is_ascii_digit() {
        digit_run_end(bytes, start, 10)
    } else {
        start
    };
    let mut end = int_end;
    let mut frac = None;
    if at(int_end) == b'.' {
        if at(int_end + 1) == b'.' {
            // scan.l `numericfail`: `1..` is the integer, and the `..` is rescanned.
            let n = integer_magnitude(&sql[start..int_end], &sql[start..int_end], 10)?;
            return Ok((Token::Int(n), int_end));
        }
        let frac_start = int_end + 1;
        // A fraction is a decinteger: it starts with a digit (a `_` right after the `.` is junk).
        end = if at(frac_start).is_ascii_digit() {
            digit_run_end(bytes, frac_start, 10)
        } else {
            frac_start
        };
        frac = Some(&sql[frac_start..end]);
    }
    // An optional exponent `[eE][+-]?decinteger`; a bare `e` is junk below, `e+` with no digit is
    // junk here (scan.l `realfail`).
    let mut exp = None;
    if matches!(at(end), b'e' | b'E') {
        let neg = at(end + 1) == b'-';
        let signed = neg || at(end + 1) == b'+';
        let digits_start = end + 1 + usize::from(signed);
        if at(digits_start).is_ascii_digit() {
            let exp_end = digit_run_end(bytes, digits_start, 10);
            // Clamp the magnitude to `EXP_LIMIT` while accumulating (keeps it in `i64` and bounds
            // the coefficient the shared builder may materialize).
            let mut v: i64 = 0;
            for b in bytes[digits_start..exp_end].iter().filter(|&&b| b != b'_') {
                if v < EXP_LIMIT {
                    v = (v * 10 + i64::from(b - b'0')).min(EXP_LIMIT);
                }
            }
            exp = Some(if neg { -v } else { v });
            end = exp_end;
        } else if signed {
            return Err(junk(digits_start));
        }
    }
    let glued = ident_len(bytes, end);
    if glued > 0 {
        return Err(junk(end + glued));
    }
    let int_part = strip_underscores(&sql[start..int_end]);
    if frac.is_none() && exp.is_none() {
        let n = integer_magnitude(&sql[start..end], &int_part, 10)?;
        return Ok((Token::Int(n), end));
    }
    let frac = strip_underscores(frac.unwrap_or(""));
    let (digits, scale) = decimal_from_parts(&int_part, &frac, exp);
    Ok((Token::Decimal(digits, scale), end))
}

/// Tokenize `sql` into tokens terminated by `Token::Eof`. Whitespace separates
/// tokens; an integer literal is an unsigned magnitude (the leading `-`, if any, is
/// the `Minus` operator, folded by the parser). Errors are structured (SQLSTATE
/// 42601 syntax error).
///
/// This is the stored-text entry point (a view body, a CHECK expression, a rename rewrite), so an
/// error carries no position: it would point into text the host never wrote (§10.1).
/// The largest bind-parameter index a statement may name (`$65535`, spec/design/api.md §5):
/// PostgreSQL's wire-protocol limit on the parameters of one statement. A fixed, cross-core-identical
/// bound, like `MAX_EXPR_DEPTH` — the parameter table a statement allocates is sized by its largest
/// index, so the bound is what keeps `SELECT $4294967295` from allocating four billion slots.
pub const MAX_PARAM_INDEX: u64 = 65535;

pub fn lex(sql: &str) -> Result<Vec<Token>> {
    lex_with_positions(sql)
        .map(|(tokens, _)| tokens)
        .map_err(|mut e| {
            e.position = None;
            e
        })
}

/// [`lex`], also returning each token's 1-based **character** position in `sql`
/// (spec/design/error-fields.md §10.1), index-aligned with the tokens. `Eof`'s position is
/// one past the last character — PostgreSQL's "at end of input". An error carries the position of
/// the lexeme being scanned (spec/design/error-fields.md §10.2).
pub fn lex_with_positions(sql: &str) -> Result<(Vec<Token>, Vec<u32>)> {
    // The text is checked as a whole before it is scanned, as PostgreSQL verifies a query string's
    // encoding before parsing it (pg_verifymbstr). A `&str` is valid UTF-8 by construction, so the
    // one check left is the NUL byte: a text value never holds one (spec/design/types.md §11), and
    // the scan copies literal and identifier bytes raw — so a NUL anywhere in the text is 22021,
    // with no position, exactly as the Go core reports it; a host text parameter is checked the
    // same way at bind (api.md §5).
    check_text_encoding(sql)?;
    let mut starts = Vec::new();
    let mut tok_start = 0;
    match lex_raw(sql, &mut starts, &mut tok_start) {
        Ok(tokens) => Ok((tokens, char_positions(sql, &starts))),
        Err(e) => {
            let pos = char_positions(sql, &[tok_start])[0];
            Err(e.with_position(pos))
        }
    }
}

/// Check text the engine receives from outside — SQL text, a host text parameter — as PostgreSQL's
/// `pg_verifymbstr` does: a NUL byte is `22021`, naming the byte as PostgreSQL's
/// `report_invalid_encoding` does (an invalid UTF-8 sequence cannot reach a `&str`).
pub(crate) fn check_text_encoding(text: &str) -> Result<()> {
    if text.as_bytes().contains(&0) {
        return Err(EngineError::new(
            SqlState::CharacterNotInRepertoire,
            "invalid byte sequence for encoding \"UTF8\": 0x00".to_string(),
        ));
    }
    Ok(())
}

/// Convert ascending byte offsets into `sql` to 1-based character positions in one pass. A
/// character is a code point: count the bytes that are not UTF-8 continuation bytes.
pub(crate) fn char_positions(sql: &str, starts: &[usize]) -> Vec<u32> {
    let bytes = sql.as_bytes();
    let mut out = Vec::with_capacity(starts.len());
    let (mut b, mut chars) = (0usize, 0u32);
    for &s in starts {
        while b < s {
            if bytes[b] & 0xC0 != 0x80 {
                chars += 1;
            }
            b += 1;
        }
        out.push(chars + 1);
    }
    out
}

/// The lexer proper: pushes each token's start byte offset to `starts`, and keeps `tok_start` at
/// the byte where the lexeme being scanned began, so the caller can position an error.
fn lex_raw(sql: &str, starts: &mut Vec<usize>, tok_start: &mut usize) -> Result<Vec<Token>> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut tokens = Vec::new();
    // Indices of the `U&'…'` / `U&"…"` tokens, decoded after the scan (`apply_unicode_escapes`).
    let mut unicode_escapes: Vec<usize> = Vec::new();

    while i < bytes.len() {
        // The previous iteration's tokens started at its `tok_start`; this one's start at `i`.
        starts.resize(tokens.len(), *tok_start);
        *tok_start = i;
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            b'(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            b'[' => {
                tokens.push(Token::LBracket);
                i += 1;
            }
            b']' => {
                tokens.push(Token::RBracket);
                i += 1;
            }
            b'*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            b'+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            b'-' => {
                // `-|-` is the range adjacency operator (range-functions.md §3), scanned greedily and
                // checked FIRST so it is never mistaken for `-` (Minus) `|-`. Its middle `|` keeps it
                // disjoint from the `--` line comment (which needs a second `-`).
                if bytes.get(i + 1) == Some(&b'|') && bytes.get(i + 2) == Some(&b'-') {
                    tokens.push(Token::Adjacent);
                    i += 3;
                } else if bytes.get(i + 1) == Some(&b'>') && bytes.get(i + 2) == Some(&b'>') {
                    // `->>` is the jsonb accessor-as-text operator (json-sql-functions.md §1),
                    // scanned greedily BEFORE `->` so it is never `-> >`.
                    tokens.push(Token::ArrowText);
                    i += 3;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    // `->` is the jsonb accessor operator (json-sql-functions.md §1).
                    tokens.push(Token::Arrow);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'-') {
                    // `--` starts a line comment running to the end of the line; comments are
                    // whitespace (grammar.md §33). Two hyphens ALWAYS start a comment outside a
                    // string, even abutting a token (`1--2` is `1` — PG behavior).
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                        i += 1;
                    }
                } else {
                    tokens.push(Token::Minus);
                    i += 1;
                }
            }
            b'/' => {
                // `/*` starts a block comment; blocks NEST (PG / the SQL standard), so a depth
                // counter tracks open/close pairs. End of input at depth >= 1 is 42601
                // (grammar.md §33). A `*/` with no opener is NOT comment syntax — it lexes as
                // `*` `/` and fails at parse.
                if bytes.get(i + 1) == Some(&b'*') {
                    i += 2;
                    let mut depth = 1;
                    while depth > 0 {
                        if i + 1 >= bytes.len() {
                            return Err(syntax("unterminated /* comment".to_string()));
                        }
                        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                            depth += 1;
                            i += 2;
                        } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                            depth -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    tokens.push(Token::Slash);
                    i += 1;
                }
            }
            b'%' => {
                tokens.push(Token::Percent);
                i += 1;
            }
            b'|' => {
                // `||` is the array concatenation operator (grammar.md §39), scanned greedily as one
                // token; a lone `|` is not part of jed's surface (no bitwise-or) — 42601.
                if bytes.get(i + 1) == Some(&b'|') {
                    tokens.push(Token::Concat);
                    i += 2;
                } else {
                    return Err(syntax("unexpected character '|'".to_string()));
                }
            }
            b':' => {
                // `::` is the PostgreSQL typecast operator (grammar.md §37), scanned greedily as
                // one token; a lone `:` is the array-slice separator `a[m:n]` (array.md §6).
                if bytes.get(i + 1) == Some(&b':') {
                    tokens.push(Token::DoubleColon);
                    i += 2;
                } else {
                    tokens.push(Token::Colon);
                    i += 1;
                }
            }
            b'=' => {
                // `=>` is the named-argument arrow (grammar.md §17), scanned greedily as one
                // token; a bare `=` is the equality operator.
                if bytes.get(i + 1) == Some(&b'>') {
                    tokens.push(Token::FatArrow);
                    i += 2;
                } else {
                    tokens.push(Token::Eq);
                    i += 1;
                }
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Le);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    // `<>` is the not-equal operator (grammar.md §4), scanned greedily; its `!=`
                    // alias is handled in the `!` arm and folds to the same token.
                    tokens.push(Token::Ne);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'@') {
                    // `<@` is the array contained-by operator (grammar.md §40), scanned greedily.
                    tokens.push(Token::ContainedBy);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'<') {
                    // `<<` is the range strictly-left operator (range-functions.md §3), scanned greedily.
                    tokens.push(Token::StrictlyLeft);
                    i += 2;
                } else {
                    tokens.push(Token::Lt);
                    i += 1;
                }
            }
            b'!' => {
                // `!~*` / `!~` are the negated regex match operators (grammar.md §22b), checked
                // greedily BEFORE the `!=`→`Ne` alias and the lone-`!` error. `!=` is the
                // PostgreSQL alias for `<>` (grammar.md §4); both fold to `Token::Ne`. A lone `!`
                // is not part of jed's surface (no factorial / boolean-not) — 42601.
                if bytes.get(i + 1) == Some(&b'~') && bytes.get(i + 2) == Some(&b'*') {
                    tokens.push(Token::BangTildeStar);
                    i += 3;
                } else if bytes.get(i + 1) == Some(&b'~') {
                    tokens.push(Token::BangTilde);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'=') {
                    tokens.push(Token::Ne);
                    i += 2;
                } else {
                    return Err(syntax("unexpected character '!'".to_string()));
                }
            }
            b'~' => {
                // `~*` is the case-insensitive regex match operator (grammar.md §22b), scanned
                // greedily as one token (never `~` `Star`); a bare `~` is the case-sensitive form.
                if bytes.get(i + 1) == Some(&b'*') {
                    tokens.push(Token::TildeStar);
                    i += 2;
                } else {
                    tokens.push(Token::Tilde);
                    i += 1;
                }
            }
            b'@' => {
                // `@>` array containment (grammar.md §40); `@?` / `@@` are the jsonpath
                // exists / match operators (jsonpath.md §6). Scanned greedily; a lone `@` is not
                // jed's surface — 42601.
                if bytes.get(i + 1) == Some(&b'>') {
                    tokens.push(Token::Contains);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'?') {
                    tokens.push(Token::JsonPathExists);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'@') {
                    tokens.push(Token::JsonPathMatch);
                    i += 2;
                } else {
                    return Err(syntax("unexpected character '@'".to_string()));
                }
            }
            b'&' => {
                // `&&` is the array overlap operator (grammar.md §40); `&<` (not-extend-right) and
                // `&>` (not-extend-left) are the range positional operators (range-functions.md §3).
                // Each scanned greedily; a lone `&` is not part of jed's surface (no bitwise-and) — 42601.
                if bytes.get(i + 1) == Some(&b'&') {
                    tokens.push(Token::Overlaps);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'<') {
                    tokens.push(Token::NotExtendRight);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    tokens.push(Token::NotExtendLeft);
                    i += 2;
                } else {
                    return Err(syntax("unexpected character '&'".to_string()));
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Ge);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    // `>>` is the range strictly-right operator (range-functions.md §3), scanned greedily.
                    tokens.push(Token::StrictlyRight);
                    i += 2;
                } else {
                    tokens.push(Token::Gt);
                    i += 1;
                }
            }
            b'?' => {
                // `?|` (any-key) / `?&` (all-keys) / `?` (key-exists) are the jsonb key-existence
                // operators (json-sql-functions.md §1), scanned greedily. A lone `?` is the
                // key-exists operator (jed binds parameters as `$N`, never `?`).
                if bytes.get(i + 1) == Some(&b'|') {
                    tokens.push(Token::QuestionPipe);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'&') {
                    tokens.push(Token::QuestionAmp);
                    i += 2;
                } else {
                    tokens.push(Token::Question);
                    i += 1;
                }
            }
            b'#' => {
                // `#>>` / `#>` (get-at-path) and `#-` (delete-at-path) are the jsonb path operators
                // (json-sql-functions.md §1), scanned greedily (`#>>` before `#>`). A lone `#` is not
                // part of jed's surface — 42601.
                if bytes.get(i + 1) == Some(&b'>') && bytes.get(i + 2) == Some(&b'>') {
                    tokens.push(Token::HashArrowText);
                    i += 3;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    tokens.push(Token::HashArrow);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'-') {
                    tokens.push(Token::HashMinus);
                    i += 2;
                } else {
                    return Err(syntax("unexpected character '#'".to_string()));
                }
            }
            b'\'' => {
                // Single-quoted string literal (the `text` type). `''` is an embedded
                // single quote; backslash is an ordinary character (no C-style escapes —
                // standard_conforming_strings, spec/design/types.md §11). The input is
                // valid UTF-8 and `'` is ASCII (never a UTF-8 continuation byte), so
                // copying raw bytes between quotes preserves UTF-8 validity.
                i += 1; // consume the opening quote
                let buf = scan_quoted_body(bytes, &mut i, b'\'')
                    .ok_or_else(|| syntax("unterminated string literal".to_string()))?;
                let s = String::from_utf8(buf)
                    .map_err(|_| syntax("invalid UTF-8 in string literal".to_string()))?;
                tokens.push(Token::Str(s));
            }
            b'"' => {
                // Double-quoted identifier (spec/design/grammar.md §3): the name VERBATIM
                // (case-sensitive, any UTF-8), `""` an embedded double quote. The input is valid
                // UTF-8 and `"` is ASCII, so copying raw bytes between quotes preserves UTF-8
                // validity.
                i += 1; // consume the opening quote
                let buf = scan_quoted_body(bytes, &mut i, b'"')
                    .ok_or_else(|| syntax("unterminated quoted identifier".to_string()))?;
                // A zero-length delimited identifier is a syntax error (PostgreSQL's rule,
                // spec/design/grammar.md §3).
                if buf.is_empty() {
                    return Err(syntax("zero-length delimited identifier".to_string()));
                }
                // The identifier-length gate applies to a quoted name too, measured on its
                // decoded UTF-8 bytes (spec/design/cost.md §7).
                if buf.len() > crate::parser::MAX_IDENTIFIER_LENGTH {
                    return Err(name_too_long());
                }
                let s = String::from_utf8(buf)
                    .map_err(|_| syntax("invalid UTF-8 in quoted identifier".to_string()))?;
                tokens.push(Token::QuotedIdent(s));
            }
            b'u' | b'U'
                if bytes.get(i + 1) == Some(&b'&')
                    && matches!(bytes.get(i + 2), Some(&b'\'') | Some(&b'"')) =>
            {
                // A Unicode-escape string `U&'…'` or identifier `U&"…"` (grammar.md §3). The body
                // is scanned exactly like its plain form (`''` / `""` doubled) and kept RAW here;
                // `apply_unicode_escapes` decodes it once the whole input is lexed, because an
                // optional `UESCAPE '<c>'` after it chooses the escape character.
                let quote = bytes[i + 2];
                i += 3; // consume `U&` and the opening quote
                let buf = scan_quoted_body(bytes, &mut i, quote).ok_or_else(|| {
                    syntax(if quote == b'\'' {
                        "unterminated string literal".to_string()
                    } else {
                        "unterminated quoted identifier".to_string()
                    })
                })?;
                // The zero-length check precedes decoding (PostgreSQL's order); every escape
                // decodes to at least one character, so a non-empty body never decodes empty.
                if quote == b'"' && buf.is_empty() {
                    return Err(syntax("zero-length delimited identifier".to_string()));
                }
                let s = String::from_utf8(buf)
                    .map_err(|_| syntax("invalid UTF-8 in string literal".to_string()))?;
                unicode_escapes.push(tokens.len());
                tokens.push(if quote == b'\'' {
                    Token::Str(s)
                } else {
                    Token::QuotedIdent(s)
                });
            }
            b'0'..=b'9' => {
                // A numeric literal: an integer (decimal, or `0x`/`0o`/`0b` radix), or a decimal
                // (a `.` and/or an `e`-exponent), with `_` digit separators — PostgreSQL's scan.l
                // number rules (grammar.md §4).
                let (tok, end) = scan_number(sql, i)?;
                tokens.push(tok);
                i = end;
            }
            b'.' => {
                // A `.` has two roles, disambiguated on the FOLLOWING byte alone (no
                // preceding-token context, so the rule is trivially identical across cores —
                // spec/design/grammar.md §4): a digit immediately after starts a leading-dot
                // decimal literal (`.5`); otherwise it is the `Dot` token of a qualified column
                // reference (`t.col`, §15). The lone overlap — an identifier then `.<digit>`
                // (`t.5`) — is invalid either way (a column name is never numeric) and lexes
                // here as a decimal, rejected at parse.
                if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                    let (tok, end) = scan_number(sql, i)?;
                    tokens.push(tok);
                    i = end;
                } else {
                    tokens.push(Token::Dot);
                    i += 1;
                }
            }
            b'$' => {
                // A bind parameter `$N` — `$` then a 1-based decimal index (spec/design/api.md
                // §5, grammar.md §5). `$` with no following digit, `$0`, and a leading zero
                // (`$01`) are all 42601; so is an index above MAX_PARAM_INDEX: the parameter table
                // is sized by the largest index a statement names, so an unbounded index would be
                // an unbounded allocation (CLAUDE.md §13).
                i += 1; // consume '$'
                let digit_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let digits = &sql[digit_start..i];
                if digits.is_empty() {
                    return Err(syntax("expected a parameter number after '$'".to_string()));
                }
                if digits.as_bytes()[0] == b'0' {
                    return Err(syntax(format!(
                        "invalid parameter number ${digits}: parameters are 1-based with no leading zero"
                    )));
                }
                let n: u32 = match digits.parse::<u64>() {
                    Ok(n) if n <= MAX_PARAM_INDEX => n as u32,
                    _ => {
                        return Err(syntax(format!(
                            "parameter number out of range: ${digits} (the maximum is ${MAX_PARAM_INDEX})"
                        )));
                    }
                };
                tokens.push(Token::Param(n));
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                // Identifier-length gate (CLAUDE.md §13; spec/design/cost.md §7). A word is an
                // identifier or a keyword; identifiers are ASCII-only here (so bytes = chars), and
                // no keyword is this long, so bounding the word length bounds every identifier on
                // every parse path. Aborts with 42622 before the (possibly huge) name is interned.
                if i - start > crate::parser::MAX_IDENTIFIER_LENGTH {
                    return Err(name_too_long());
                }
                // A bare word folds to lowercase (ASCII) here, so every later comparison of a
                // name is byte-exact and keyword matching is case-insensitive by construction
                // (spec/design/grammar.md §3).
                tokens.push(Token::Word(sql[start..i].to_ascii_lowercase()));
            }
            _ => {
                return Err(syntax(format!("unexpected character '{}'", c as char)));
            }
        }
    }

    starts.resize(tokens.len(), *tok_start);
    *tok_start = bytes.len();
    tokens.push(Token::Eof);
    starts.push(bytes.len());
    if !unicode_escapes.is_empty() {
        tokens = apply_unicode_escapes(tokens, starts, &unicode_escapes, tok_start)?;
    }
    Ok(tokens)
}

/// Scan a quoted body from `bytes[*i..]` (just past the opening `quote`) through its closing quote,
/// collapsing each doubled quote to one. Returns `None` when the input ends first.
fn scan_quoted_body(bytes: &[u8], i: &mut usize, quote: u8) -> Option<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let b = *bytes.get(*i)?;
        if b == quote {
            if bytes.get(*i + 1) == Some(&quote) {
                buf.push(quote);
                *i += 2;
            } else {
                *i += 1; // consume the closing quote
                return Some(buf);
            }
        } else {
            buf.push(b);
            *i += 1;
        }
    }
}

/// Decode every `U&'…'` / `U&"…"` token (their indices in `uesc`, ascending) — PostgreSQL's
/// `base_yylex` filter and `str_udeescape` (grammar.md §3). A `UESCAPE '<c>'` right after the token
/// picks the escape character and is consumed with it; otherwise the escape is `\`. The decoded
/// token is a plain `Str` / `QuotedIdent`, so nothing downstream (and no stored text, which renders
/// it with plain quotes) ever sees the Unicode-escape spelling. An error sets `*tok_start` to the
/// byte PostgreSQL's error cursor names.
fn apply_unicode_escapes(
    tokens: Vec<Token>,
    starts: &mut Vec<usize>,
    uesc: &[usize],
    tok_start: &mut usize,
) -> Result<Vec<Token>> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut out_starts = Vec::with_capacity(starts.len());
    let mut idx = 0;
    while idx < tokens.len() {
        if uesc.binary_search(&idx).is_err() {
            out.push(tokens[idx].clone());
            out_starts.push(starts[idx]);
            idx += 1;
            continue;
        }
        let start = starts[idx];
        let mut escape = b'\\';
        let mut next = idx + 1;
        if matches!(&tokens[idx + 1], Token::Word(w) if w == "uescape") {
            // The third token must be a simple string literal of exactly one byte that is not a hex
            // digit, `+`, `'`, `"`, or whitespace. Errors point at that third token. `Eof` always
            // follows, so `idx + 2` is in range.
            let third = idx + 2;
            *tok_start = starts[third];
            let esc = match &tokens[third] {
                Token::Str(s) if uesc.binary_search(&third).is_err() => s.as_bytes(),
                _ => {
                    return Err(syntax(
                        "UESCAPE must be followed by a simple string literal".to_string(),
                    ));
                }
            };
            if esc.len() != 1 || !is_uescape_char(esc[0]) {
                return Err(syntax("invalid Unicode escape character".to_string()));
            }
            escape = esc[0];
            next = idx + 3;
        }
        let (raw, is_ident) = match &tokens[idx] {
            Token::Str(s) => (s.as_bytes(), false),
            Token::QuotedIdent(s) => (s.as_bytes(), true),
            _ => unreachable!("a Unicode-escape token is a Str or QuotedIdent"),
        };
        // PostgreSQL's cursor is the token start + 3 (`U&'`) + the offset into the body AFTER `''` /
        // `""` collapsing, measured in bytes.
        let decoded = udeescape(raw, escape).map_err(|(e, off)| {
            *tok_start = start + 3 + off;
            e
        })?;
        if is_ident && decoded.len() > crate::parser::MAX_IDENTIFIER_LENGTH {
            *tok_start = start;
            return Err(name_too_long());
        }
        // Every escape decodes to a valid scalar value, so the result stays valid UTF-8.
        let decoded = String::from_utf8(decoded).expect("decoded Unicode escapes are UTF-8");
        out.push(if is_ident {
            Token::QuotedIdent(decoded)
        } else {
            Token::Str(decoded)
        });
        out_starts.push(start);
        idx = next;
    }
    *starts = out_starts;
    Ok(out)
}

/// PostgreSQL's `check_uescapechar`: a UESCAPE character may not be a hex digit, `+`, `'`, `"`, or
/// scanner whitespace.
fn is_uescape_char(c: u8) -> bool {
    !(c.is_ascii_hexdigit()
        || c == b'+'
        || c == b'\''
        || c == b'"'
        || matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C))
}

/// The value of `n` hex digits at `raw[at..]`, or `None` if any is missing or not a hex digit.
fn hex_digits(raw: &[u8], at: usize, n: usize) -> Option<u32> {
    let mut v = 0u32;
    for k in at..at + n {
        let d = (*raw.get(k)? as char).to_digit(16)?;
        v = (v << 4) | d;
    }
    Some(v)
}

/// PostgreSQL's `str_udeescape`: replace `<esc>XXXX` / `<esc>+XXXXXX` with the code point's UTF-8
/// and `<esc><esc>` with `<esc>`, combining a UTF-16 surrogate pair written as two escapes. An error
/// carries the byte offset into `raw` that PostgreSQL's cursor names (the escape, or the byte after
/// a dangling high surrogate).
fn udeescape(raw: &[u8], esc: u8) -> std::result::Result<Vec<u8>, (EngineError, usize)> {
    let invalid_pair = |at: usize| (syntax("invalid Unicode surrogate pair".to_string()), at);
    let mut out = Vec::with_capacity(raw.len());
    let mut pair_first: u32 = 0;
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != esc {
            if pair_first != 0 {
                return Err(invalid_pair(i));
            }
            out.push(raw[i]);
            i += 1;
            continue;
        }
        if raw.get(i + 1) == Some(&esc) {
            if pair_first != 0 {
                return Err(invalid_pair(i));
            }
            out.push(esc);
            i += 2;
            continue;
        }
        let (mut cp, len) = if let Some(v) = hex_digits(raw, i + 1, 4) {
            (v, 5)
        } else if raw.get(i + 1) == Some(&b'+')
            && let Some(v) = hex_digits(raw, i + 2, 6)
        {
            (v, 8)
        } else {
            let e = syntax("invalid Unicode escape".to_string()).with_hint(Some(
                "Unicode escapes must be \\XXXX or \\+XXXXXX.".to_string(),
            ));
            return Err((e, i));
        };
        if cp == 0 || cp > 0x10FFFF {
            return Err((syntax("invalid Unicode escape value".to_string()), i));
        }
        let is_high = (0xD800..=0xDBFF).contains(&cp);
        let is_low = (0xDC00..=0xDFFF).contains(&cp);
        if pair_first != 0 {
            if !is_low {
                return Err(invalid_pair(i));
            }
            cp = (((pair_first & 0x3FF) << 10) | (cp & 0x3FF)) + 0x10000;
            pair_first = 0;
        } else if is_low {
            return Err(invalid_pair(i));
        } else if is_high {
            pair_first = cp;
            i += len;
            continue;
        }
        let ch = char::from_u32(cp).expect("a non-surrogate code point <= 0x10FFFF");
        let mut utf8 = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
        i += len;
    }
    if pair_first != 0 {
        return Err(invalid_pair(raw.len()));
    }
    Ok(out)
}

fn name_too_long() -> EngineError {
    EngineError::new(
        SqlState::NameTooLong,
        format!(
            "identifier exceeds the maximum length of {} bytes",
            crate::parser::MAX_IDENTIFIER_LENGTH
        ),
    )
}

fn syntax(msg: String) -> EngineError {
    EngineError::new(SqlState::SyntaxError, msg)
}

#[cfg(test)]
mod text_encoding_tests {
    //! Host-API behavior the corpus cannot express (CLAUDE.md §10): a NUL byte in SQL text or in a
    //! text parameter — the one encoding fault a `&str` can carry — is 22021 with PostgreSQL's
    //! message (spec/design/types.md §11), the same report the Go core gives for the same bytes.

    use super::*;
    use crate::{CreateOptions, Database, Session, SessionOptions, Value};

    fn session() -> Session {
        Database::create(CreateOptions::default())
            .unwrap()
            .session(SessionOptions::default())
    }

    #[test]
    fn nul_in_sql_text_is_22021_without_position() {
        for sql in ["SELECT 'a\0b'", "SELECT \"a\0b\"", "SELECT 1 -- \0"] {
            let err = lex_with_positions(sql).unwrap_err();
            assert_eq!(err.code(), "22021", "{sql:?}");
            assert_eq!(
                err.message,
                "invalid byte sequence for encoding \"UTF8\": 0x00"
            );
            assert_eq!(err.position, None);
        }
        // The stored-text entry point rejects it the same way.
        assert_eq!(lex("SELECT 'a\0b'").unwrap_err().code(), "22021");
    }

    #[test]
    fn nul_in_text_parameter_is_22021() {
        let mut db = session();
        db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, v text)", &[])
            .unwrap();
        let err = db
            .query_outcome(
                "INSERT INTO t VALUES (1, $1)",
                &[Value::Text("a\0b".to_string())],
            )
            .unwrap_err();
        assert_eq!(err.code(), "22021");
        assert_eq!(
            err.message,
            "invalid byte sequence for encoding \"UTF8\": 0x00"
        );
        // Nothing was stored; ordinary non-ASCII text still binds.
        db.query_outcome(
            "INSERT INTO t VALUES (1, $1)",
            &[Value::Text("h\u{e9}\u{1F600}".to_string())],
        )
        .unwrap();
    }

    #[test]
    fn parameter_index_is_capped() {
        assert!(lex("SELECT $65535").is_ok());
        let err = lex("SELECT $65536").unwrap_err();
        assert_eq!(err.code(), "42601");
        assert_eq!(
            err.message,
            "parameter number out of range: $65536 (the maximum is $65535)"
        );
        assert_eq!(lex("SELECT $4294967295").unwrap_err().code(), "42601");
        assert_eq!(
            lex("SELECT $99999999999999999999999").unwrap_err().code(),
            "42601"
        );
    }
}
