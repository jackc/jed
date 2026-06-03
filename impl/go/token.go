package jed

// TokenKind classifies a lexed token.
type TokenKind int

const (
	// TokWord is a bare word: keyword or identifier (compared case-insensitively).
	TokWord TokenKind = iota
	// TokInt is an integer literal's unsigned magnitude (the sign is TokMinus).
	TokInt
	// TokStr is a single-quoted string literal's decoded content (the text type): the
	// lexer strips the quotes and collapses each doubled '' to one ' (no backslash
	// escapes — standard_conforming_strings, spec/design/types.md §11).
	TokStr
	// TokDecimal is a decimal literal (a numeric literal containing a '.'): Word holds the
	// unscaled coefficient as a decimal-digit string (leading zeros allowed, no sign) and Int
	// holds the scale (fractional digit count). 1.50 → ("150", 2). The sign is TokMinus; the
	// cap check is at resolve (spec/design/grammar.md §14).
	TokDecimal
	// TokComma is ",".
	TokComma
	// TokLParen is "(".
	TokLParen
	// TokRParen is ")".
	TokRParen
	// TokStar is "*".
	TokStar
	// TokPlus is "+".
	TokPlus
	// TokMinus is "-".
	TokMinus
	// TokSlash is "/".
	TokSlash
	// TokPercent is "%".
	TokPercent
	// TokEq is "=".
	TokEq
	// TokLt is "<".
	TokLt
	// TokGt is ">".
	TokGt
	// TokLe is "<=".
	TokLe
	// TokGe is ">=".
	TokGe
	// TokEof marks end of input.
	TokEof
)

// Token is a lexed token. Word holds the text for TokWord; Int holds the unsigned
// magnitude for TokInt. The lexer guarantees the magnitude is <= 2^63; int64 cannot
// hold 2^63, so the parser converts — a bare magnitude > MaxInt64 traps 22003, and
// -(2^63) folds to int64's minimum (spec/design/grammar.md §4).
type Token struct {
	Kind TokenKind
	Word string // TokWord, or the decoded string for TokStr
	Int  uint64
}
