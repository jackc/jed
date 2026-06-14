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
    /// A decimal literal (a numeric literal containing a `.`): the unscaled coefficient as a
    /// decimal-digit string (leading zeros allowed, no sign) and the scale (fractional digit
    /// count). `1.50` → `("150", 2)`, `.5` → `("5", 1)`, `1.` → `("1", 0)`. The sign is the
    /// `Minus` operator; the cap check is at resolve (spec/design/grammar.md §14).
    Decimal(String, u32),
    /// A bind parameter `$N` — its 1-based index. The lexer rejects `$0`, a leading zero
    /// (`$01`), and `$` not followed by a digit (42601). Bound by the host API, not the
    /// corpus (spec/design/api.md, grammar.md §5).
    Param(u32),
    Comma,
    /// The `.` separator of a qualified column reference (`t.col`). Emitted only when a
    /// `.` is NOT part of a numeric literal — i.e. with no digit immediately after it
    /// (spec/design/grammar.md §4/§15).
    Dot,
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
    /// The `::` PostgreSQL typecast operator (`expr::type` = `CAST(expr AS type)`). Two colons,
    /// scanned greedily; a lone `:` is a 42601 syntax error. See spec/design/grammar.md §37.
    DoubleColon,
    /// The `=>` named-argument arrow (`name => value`, PostgreSQL named notation). Two chars,
    /// scanned greedily after `=`; the legacy `:=` spelling is not part of jed's surface. See
    /// spec/design/grammar.md §17.
    FatArrow,
    /// End of input.
    Eof,
}
