//! Hand-written lexer for the step-1 SQL surface (CLAUDE.md §5: parsers are
//! per-language, not codegen'd). Produces a flat token vector.

use crate::error::{EngineError, Result, SqlState};
use crate::token::Token;

/// Tokenize `sql` into tokens terminated by `Token::Eof`. Whitespace separates
/// tokens; integer literals may carry a leading `-`. Errors are structured
/// (SQLSTATE 42601 syntax error).
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
            b'-' | b'0'..=b'9' => {
                // Integer literal. A leading '-' is part of the number only when
                // followed by a digit; otherwise it is unsupported punctuation.
                let start = i;
                if c == b'-' {
                    if !(i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit()) {
                        return Err(syntax(format!("unexpected character '{}'", c as char)));
                    }
                    i += 1;
                }
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let text = &sql[start..i];
                let n: i64 = text
                    .parse()
                    .map_err(|_| syntax(format!("integer literal out of range: {text}")))?;
                tokens.push(Token::Int(n));
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
