package abide

// TokenKind classifies a lexed token.
type TokenKind int

const (
	// TokWord is a bare word: keyword or identifier (compared case-insensitively).
	TokWord TokenKind = iota
	// TokInt is an integer literal already parsed to int64.
	TokInt
	// TokComma is ",".
	TokComma
	// TokLParen is "(".
	TokLParen
	// TokRParen is ")".
	TokRParen
	// TokStar is "*".
	TokStar
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

// Token is a lexed token. Word holds the text for TokWord; Int holds the value for
// TokInt.
type Token struct {
	Kind TokenKind
	Word string
	Int  int64
}
