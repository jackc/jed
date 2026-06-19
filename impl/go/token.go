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
	// TokDot is the "." separator of a qualified column reference (t.col). Emitted only when
	// a "." is NOT part of a numeric literal — i.e. with no digit immediately after it
	// (spec/design/grammar.md §4/§15).
	TokDot
	// TokLParen is "(".
	TokLParen
	// TokRParen is ")".
	TokRParen
	// TokLBracket is "[" — the array subscript / ARRAY[…] / T[] type-suffix bracket
	// (spec/design/array.md).
	TokLBracket
	// TokRBracket is "]".
	TokRBracket
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
	// TokNe is "<>" (or its "!=" alias) — the not-equal operator. The lexer folds both
	// spellings to this one token (spec/design/grammar.md §4).
	TokNe
	// TokLt is "<".
	TokLt
	// TokGt is ">".
	TokGt
	// TokLe is "<=".
	TokLe
	// TokGe is ">=".
	TokGe
	// TokDoubleColon is the "::" PostgreSQL typecast operator (expr::type = CAST(expr AS type)).
	// Two colons, scanned greedily (spec/design/grammar.md §37).
	TokDoubleColon
	// TokColon is a single ":" — the array-slice bound separator a[m:n] (spec/design/array.md §6).
	TokColon
	// TokFatArrow is the "=>" named-argument arrow (name => value, PostgreSQL named notation).
	// Two chars, scanned greedily after "="; the legacy ":=" is not part of jed's surface
	// (spec/design/grammar.md §17).
	TokFatArrow
	// TokConcat is the "||" array concatenation operator (a || b). Two "|" scanned greedily; a lone
	// "|" is a 42601 syntax error (jed has no bitwise-or). spec/design/grammar.md §39.
	TokConcat
	// TokContains is the "@>" array containment operator (a @> b — does a contain b). "@" then ">"
	// scanned greedily; a lone "@" is a 42601 syntax error. spec/design/grammar.md §40.
	TokContains
	// TokContainedBy is the "<@" array contained-by operator (a <@ b — is a contained by b). "<"
	// then "@". spec/design/grammar.md §40.
	TokContainedBy
	// TokOverlaps is the "&&" array overlap operator (a && b — do a and b share an element). Two "&"
	// scanned greedily; a lone "&" is a 42601 syntax error (no bitwise-and). spec/design/grammar.md §40.
	TokOverlaps
	// TokParam is a bind parameter $N — Int holds the 1-based index. The lexer rejects $0, a
	// leading zero ($01), and $ with no following digit (42601). Bound by the host API, not the
	// corpus (spec/design/api.md, grammar.md §5).
	TokParam
	// TokEof marks end of input.
	TokEof
)

// Token is a lexed token. Word holds the text for TokWord; Int holds the unsigned
// magnitude for TokInt. The lexer guarantees the magnitude is <= 2^63; i64 cannot
// hold 2^63, so the parser converts — a bare magnitude > MaxInt64 traps 22003, and
// -(2^63) folds to i64's minimum (spec/design/grammar.md §4).
type Token struct {
	Kind TokenKind
	Word string // TokWord, or the decoded string for TokStr
	Int  uint64
}
