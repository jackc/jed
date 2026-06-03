//! Tokens for the step-1 SQL lexer.

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Token {
    /// A bare word: keyword or identifier (callers compare case-insensitively).
    Word(String),
    /// An integer literal's UNSIGNED magnitude (the sign is the `Minus` operator).
    /// The lexer guarantees it is `<= 2^63`; `i64`/`int64` cannot hold `2^63`, so the
    /// parser converts: a bare magnitude `> i64::MAX` traps 22003, and `-(2^63)` folds
    /// to `int64::MIN`. See spec/design/grammar.md §4.
    Int(u64),
    /// A single-quoted string literal's decoded content (the `text` type). The lexer
    /// strips the surrounding quotes and collapses each doubled `''` to one `'`
    /// (standard_conforming_strings; no backslash escapes). See spec/design/types.md §11.
    Str(String),
    Comma,
    LParen,
    RParen,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    Lt,
    Gt,
    Le,
    Ge,
    /// End of input.
    Eof,
}
