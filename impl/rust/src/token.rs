//! Tokens for the step-1 SQL lexer.

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Token {
    /// A bare word: keyword or identifier (callers compare case-insensitively).
    Word(String),
    /// An integer literal already parsed to i64.
    Int(i64),
    Comma,
    LParen,
    RParen,
    Star,
    Eq,
    Lt,
    Gt,
    Le,
    Ge,
    /// End of input.
    Eof,
}
