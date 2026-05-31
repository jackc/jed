package abide

import (
	"fmt"
	"strconv"
)

// Lex tokenizes sql into tokens terminated by TokEof (CLAUDE.md §5: parsers are
// per-language, not codegen'd). Integer literals may carry a leading '-'. Errors are
// structured (SQLSTATE 42601 syntax error).
func Lex(sql string) ([]Token, error) {
	b := []byte(sql)
	i := 0
	var tokens []Token

	isDigit := func(c byte) bool { return c >= '0' && c <= '9' }
	isAlpha := func(c byte) bool {
		return c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
	}

	for i < len(b) {
		c := b[i]
		switch {
		case c == ' ' || c == '\t' || c == '\r' || c == '\n':
			i++
		case c == ',':
			tokens = append(tokens, Token{Kind: TokComma})
			i++
		case c == '(':
			tokens = append(tokens, Token{Kind: TokLParen})
			i++
		case c == ')':
			tokens = append(tokens, Token{Kind: TokRParen})
			i++
		case c == '*':
			tokens = append(tokens, Token{Kind: TokStar})
			i++
		case c == '=':
			tokens = append(tokens, Token{Kind: TokEq})
			i++
		case c == '<':
			if i+1 < len(b) && b[i+1] == '=' {
				tokens = append(tokens, Token{Kind: TokLe})
				i += 2
			} else {
				tokens = append(tokens, Token{Kind: TokLt})
				i++
			}
		case c == '>':
			if i+1 < len(b) && b[i+1] == '=' {
				tokens = append(tokens, Token{Kind: TokGe})
				i += 2
			} else {
				tokens = append(tokens, Token{Kind: TokGt})
				i++
			}
		case c == '-' || isDigit(c):
			// Integer literal. A leading '-' is part of the number only when
			// followed by a digit.
			start := i
			if c == '-' {
				if !(i+1 < len(b) && isDigit(b[i+1])) {
					return nil, NewError(SyntaxError, fmt.Sprintf("unexpected character '%c'", c))
				}
				i++
			}
			for i < len(b) && isDigit(b[i]) {
				i++
			}
			text := sql[start:i]
			n, err := strconv.ParseInt(text, 10, 64)
			if err != nil {
				return nil, NewError(SyntaxError, fmt.Sprintf("integer literal out of range: %s", text))
			}
			tokens = append(tokens, Token{Kind: TokInt, Int: n})
		case isAlpha(c):
			start := i
			for i < len(b) && (isAlpha(b[i]) || isDigit(b[i])) {
				i++
			}
			tokens = append(tokens, Token{Kind: TokWord, Word: sql[start:i]})
		default:
			return nil, NewError(SyntaxError, fmt.Sprintf("unexpected character '%c'", c))
		}
	}

	tokens = append(tokens, Token{Kind: TokEof})
	return tokens, nil
}
