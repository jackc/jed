//! Hand-written lexer for the step-1 SQL surface (CLAUDE.md §5: parsers are
//! per-language, not codegen'd). Produces a flat token vector.

use crate::decimal::{EXP_LIMIT, decimal_from_parts};
use crate::error::{EngineError, Result, SqlState};
use crate::token::Token;

/// If `bytes[*i..]` begins a well-formed exponent `[eE][+-]?digit+`, consume it (advancing
/// `*i`) and return `Some(exponent)` with the magnitude clamped to `±EXP_LIMIT`. Otherwise
/// leave `*i` unchanged and return `None` — a bare `e` / `ex` is NOT part of the number (it
/// lexes as the following token, exactly as before e-notation existed; PostgreSQL likewise
/// rejects `1e` as trailing junk rather than reading it as the number `1`).
fn scan_exponent(bytes: &[u8], i: &mut usize) -> Option<i64> {
    let start = *i;
    if start >= bytes.len() || (bytes[start] != b'e' && bytes[start] != b'E') {
        return None;
    }
    let mut j = start + 1;
    let mut neg = false;
    if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
        neg = bytes[j] == b'-';
        j += 1;
    }
    if j >= bytes.len() || !bytes[j].is_ascii_digit() {
        return None; // not a valid exponent — leave `e` for the next token
    }
    let mut exp: i64 = 0;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        if exp < EXP_LIMIT {
            exp = exp * 10 + (bytes[j] - b'0') as i64;
            if exp > EXP_LIMIT {
                exp = EXP_LIMIT;
            }
        }
        j += 1;
    }
    *i = j;
    Some(if neg { -exp } else { exp })
}

/// Tokenize `sql` into tokens terminated by `Token::Eof`. Whitespace separates
/// tokens; an integer literal is an unsigned magnitude (the leading `-`, if any, is
/// the `Minus` operator, folded by the parser). Errors are structured (SQLSTATE
/// 42601 syntax error).
pub fn lex(sql: &str) -> Result<Vec<Token>> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut tokens = Vec::new();

    while i < bytes.len() {
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
                // `!=` is the PostgreSQL alias for `<>` (grammar.md §4); both fold to `Token::Ne`.
                // A lone `!` is not part of jed's surface (no factorial / boolean-not) — 42601.
                if bytes.get(i + 1) == Some(&b'=') {
                    tokens.push(Token::Ne);
                    i += 2;
                } else {
                    return Err(syntax("unexpected character '!'".to_string()));
                }
            }
            b'@' => {
                // `@>` is the array containment operator (grammar.md §40), scanned greedily as one
                // token; a lone `@` is not part of jed's surface — 42601.
                if bytes.get(i + 1) == Some(&b'>') {
                    tokens.push(Token::Contains);
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
            b'\'' => {
                // Single-quoted string literal (the `text` type). `''` is an embedded
                // single quote; backslash is an ordinary character (no C-style escapes —
                // standard_conforming_strings, spec/design/types.md §11). The input is
                // valid UTF-8 and `'` is ASCII (never a UTF-8 continuation byte), so
                // copying raw bytes between quotes preserves UTF-8 validity.
                i += 1; // consume the opening quote
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    match bytes.get(i) {
                        None => return Err(syntax("unterminated string literal".to_string())),
                        Some(&b'\'') => {
                            if bytes.get(i + 1) == Some(&b'\'') {
                                buf.push(b'\'');
                                i += 2;
                            } else {
                                i += 1; // consume the closing quote
                                break;
                            }
                        }
                        Some(&b) => {
                            buf.push(b);
                            i += 1;
                        }
                    }
                }
                let s = String::from_utf8(buf)
                    .map_err(|_| syntax("invalid UTF-8 in string literal".to_string()))?;
                tokens.push(Token::Str(s));
            }
            b'"' => {
                // Double-quoted identifier (collation names, spec/design/collation.md §1). `""`
                // is an embedded double quote; the content is kept VERBATIM (case-sensitive). The
                // input is valid UTF-8 and `"` is ASCII, so copying raw bytes between quotes
                // preserves UTF-8 validity. Empty (`""`) is allowed by the lexer; the parser
                // rejects an empty collation name.
                i += 1; // consume the opening quote
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    match bytes.get(i) {
                        None => return Err(syntax("unterminated quoted identifier".to_string())),
                        Some(&b'"') => {
                            if bytes.get(i + 1) == Some(&b'"') {
                                buf.push(b'"');
                                i += 2;
                            } else {
                                i += 1; // consume the closing quote
                                break;
                            }
                        }
                        Some(&b) => {
                            buf.push(b);
                            i += 1;
                        }
                    }
                }
                let s = String::from_utf8(buf)
                    .map_err(|_| syntax("invalid UTF-8 in quoted identifier".to_string()))?;
                tokens.push(Token::QuotedIdent(s));
            }
            b'0'..=b'9' => {
                // A numeric literal. Scan the integer digits; a following `.` and/or scientific
                // `e`-notation (`123.45`, `5e2`, `1.5e-3`) makes it a DECIMAL literal, otherwise
                // it is an INTEGER literal.
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let int_part = &sql[start..i];
                // Optional fractional part: `123.`, `123.45`. The fractional part may be empty.
                let mut frac = "";
                let mut has_frac = false;
                if i < bytes.len() && bytes[i] == b'.' {
                    has_frac = true;
                    i += 1; // consume '.'
                    let frac_start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    frac = &sql[frac_start..i];
                }
                // Optional exponent (`e3`, `E+2`, `e-10`). Only a well-formed exponent is
                // consumed; an exponent (even with no `.`) makes the literal a decimal.
                let exp = scan_exponent(bytes, &mut i);
                if has_frac || exp.is_some() {
                    let (digits, scale) = decimal_from_parts(int_part, frac, exp);
                    tokens.push(Token::Decimal(digits, scale));
                } else {
                    // Integer literal: an unsigned magnitude. The sign is the `Minus`
                    // operator. The magnitude must be <= 2^63 so that -(2^63) = i64::MIN
                    // is reachable; anything larger cannot be represented (42601). i64
                    // cannot hold 2^63, so carry it unsigned and let the parser convert.
                    let text = int_part;
                    let n: u64 = text
                        .parse()
                        .map_err(|_| syntax(format!("integer literal out of range: {text}")))?;
                    if n > (1u64 << 63) {
                        return Err(syntax(format!("integer literal out of range: {text}")));
                    }
                    tokens.push(Token::Int(n));
                }
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
                    i += 1; // consume '.'
                    let frac_start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    let frac = &sql[frac_start..i];
                    // A leading-dot decimal may also carry an exponent (`.5e2`).
                    let exp = scan_exponent(bytes, &mut i);
                    let (digits, scale) = decimal_from_parts("", frac, exp);
                    tokens.push(Token::Decimal(digits, scale));
                } else {
                    tokens.push(Token::Dot);
                    i += 1;
                }
            }
            b'$' => {
                // A bind parameter `$N` — `$` then a 1-based decimal index (spec/design/api.md
                // §5, grammar.md §5). `$` with no following digit, `$0`, and a leading zero
                // (`$01`) are all 42601; an index overflowing u32 is too.
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
                let n: u32 = digits
                    .parse()
                    .map_err(|_| syntax(format!("parameter number out of range: ${digits}")))?;
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
                    return Err(EngineError::new(
                        SqlState::NameTooLong,
                        format!(
                            "identifier exceeds the maximum length of {} bytes",
                            crate::parser::MAX_IDENTIFIER_LENGTH
                        ),
                    ));
                }
                tokens.push(Token::Word(sql[start..i].to_string()));
            }
            _ => {
                return Err(syntax(format!("unexpected character '{}'", c as char)));
            }
        }
    }

    tokens.push(Token::Eof);
    Ok(tokens)
}

fn syntax(msg: String) -> EngineError {
    EngineError::new(SqlState::SyntaxError, msg)
}
