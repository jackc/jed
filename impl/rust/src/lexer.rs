//! Hand-written lexer for the step-1 SQL surface (CLAUDE.md §5: parsers are
//! per-language, not codegen'd). Produces a flat token vector.

use crate::error::{EngineError, Result, SqlState};
use crate::token::Token;

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
            b'*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            b'+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            b'-' => {
                // `--` starts a line comment running to the end of the line; comments are
                // whitespace (grammar.md §33). Two hyphens ALWAYS start a comment outside a
                // string, even abutting a token (`1--2` is `1` — PG behavior).
                if bytes.get(i + 1) == Some(&b'-') {
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
            b':' => {
                // `::` is the PostgreSQL typecast operator (grammar.md §37), scanned greedily as
                // one token. A lone `:` is not part of jed's surface — a 42601 syntax error.
                if bytes.get(i + 1) == Some(&b':') {
                    tokens.push(Token::DoubleColon);
                    i += 2;
                } else {
                    return Err(syntax("unexpected character ':'".to_string()));
                }
            }
            b'=' => {
                tokens.push(Token::Eq);
                i += 1;
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Le);
                    i += 2;
                } else {
                    tokens.push(Token::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Ge);
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
            b'0'..=b'9' => {
                // A numeric literal. Scan the integer digits; if a `.` follows it is a
                // DECIMAL literal (scan the fractional digits), else an INTEGER literal.
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'.' {
                    // Decimal: `123.`, `123.45`. The fractional part may be empty (`1.`).
                    let int_part = &sql[start..i];
                    i += 1; // consume '.'
                    let frac_start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    let frac = &sql[frac_start..i];
                    let digits = format!("{int_part}{frac}");
                    tokens.push(Token::Decimal(digits, frac.len() as u32));
                } else {
                    // Integer literal: an unsigned magnitude. The sign is the `Minus`
                    // operator. The magnitude must be <= 2^63 so that -(2^63) = int64::MIN
                    // is reachable; anything larger cannot be represented (42601). i64
                    // cannot hold 2^63, so carry it unsigned and let the parser convert.
                    let text = &sql[start..i];
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
                    tokens.push(Token::Decimal(frac.to_string(), frac.len() as u32));
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
