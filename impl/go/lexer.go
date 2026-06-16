package jed

import (
	"fmt"
	"strconv"
)

// scanExponent: if b[i:] begins a well-formed exponent [eE][+-]?digit+, consume it and return
// (exp, true, nextIndex) with the magnitude clamped to ±expLimit. Otherwise return (0, false, i)
// — a bare e / ex is NOT part of the number (it lexes as the following token, exactly as before
// e-notation existed; PostgreSQL likewise rejects 1e as trailing junk rather than the number 1).
func scanExponent(b []byte, i int) (int64, bool, int) {
	if i >= len(b) || (b[i] != 'e' && b[i] != 'E') {
		return 0, false, i
	}
	j := i + 1
	neg := false
	if j < len(b) && (b[j] == '+' || b[j] == '-') {
		neg = b[j] == '-'
		j++
	}
	if j >= len(b) || b[j] < '0' || b[j] > '9' {
		return 0, false, i // not a valid exponent — leave e for the next token
	}
	var exp int64
	for j < len(b) && b[j] >= '0' && b[j] <= '9' {
		if exp < expLimit {
			exp = exp*10 + int64(b[j]-'0')
			if exp > expLimit {
				exp = expLimit
			}
		}
		j++
	}
	if neg {
		exp = -exp
	}
	return exp, true, j
}

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
		case c == '[':
			tokens = append(tokens, Token{Kind: TokLBracket})
			i++
		case c == ']':
			tokens = append(tokens, Token{Kind: TokRBracket})
			i++
		case c == '*':
			tokens = append(tokens, Token{Kind: TokStar})
			i++
		case c == '+':
			tokens = append(tokens, Token{Kind: TokPlus})
			i++
		case c == '-':
			// `--` starts a line comment running to the end of the line; comments are
			// whitespace (grammar.md §33). Two hyphens ALWAYS start a comment outside a
			// string, even abutting a token (`1--2` is `1` — PG behavior).
			if i+1 < len(b) && b[i+1] == '-' {
				i += 2
				for i < len(b) && b[i] != '\n' && b[i] != '\r' {
					i++
				}
			} else {
				tokens = append(tokens, Token{Kind: TokMinus})
				i++
			}
		case c == '/':
			// `/*` starts a block comment; blocks NEST (PG / the SQL standard), so a depth
			// counter tracks open/close pairs. End of input at depth >= 1 is 42601
			// (grammar.md §33). A `*/` with no opener is NOT comment syntax — it lexes as
			// `*` `/` and fails at parse.
			if i+1 < len(b) && b[i+1] == '*' {
				i += 2
				depth := 1
				for depth > 0 {
					if i+1 >= len(b) {
						return nil, NewError(SyntaxError, "unterminated /* comment")
					}
					switch {
					case b[i] == '/' && b[i+1] == '*':
						depth++
						i += 2
					case b[i] == '*' && b[i+1] == '/':
						depth--
						i += 2
					default:
						i++
					}
				}
			} else {
				tokens = append(tokens, Token{Kind: TokSlash})
				i++
			}
		case c == '%':
			tokens = append(tokens, Token{Kind: TokPercent})
			i++
		case c == ':':
			// `::` is the PostgreSQL typecast operator (grammar.md §37), scanned greedily as one
			// token. A lone `:` is not part of jed's surface — a 42601 syntax error.
			if i+1 < len(b) && b[i+1] == ':' {
				tokens = append(tokens, Token{Kind: TokDoubleColon})
				i += 2
			} else {
				return nil, NewError(SyntaxError, "unexpected character ':'")
			}
		case c == '=':
			// `=>` is the named-argument arrow (grammar.md §17), scanned greedily as one token;
			// a bare `=` is the equality operator.
			if i+1 < len(b) && b[i+1] == '>' {
				tokens = append(tokens, Token{Kind: TokFatArrow})
				i += 2
			} else {
				tokens = append(tokens, Token{Kind: TokEq})
				i++
			}
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
		case c == '\'':
			// Single-quoted string literal (the text type). `''` is an embedded single
			// quote; backslash is an ordinary character (no C-style escapes —
			// standard_conforming_strings, spec/design/types.md §11). The input is valid
			// UTF-8 and `'` is ASCII (never a UTF-8 continuation byte), so copying raw bytes
			// between quotes preserves UTF-8 validity.
			i++ // consume the opening quote
			var sb []byte
			closed := false
			for i < len(b) {
				if b[i] == '\'' {
					if i+1 < len(b) && b[i+1] == '\'' {
						sb = append(sb, '\'')
						i += 2
						continue
					}
					i++ // consume the closing quote
					closed = true
					break
				}
				sb = append(sb, b[i])
				i++
			}
			if !closed {
				return nil, NewError(SyntaxError, "unterminated string literal")
			}
			tokens = append(tokens, Token{Kind: TokStr, Word: string(sb)})
		case isDigit(c):
			// A numeric literal. Scan the integer digits; a following '.' and/or scientific
			// e-notation (`123.45`, `5e2`, `1.5e-3`) makes it a DECIMAL literal, otherwise an
			// INTEGER literal.
			start := i
			for i < len(b) && isDigit(b[i]) {
				i++
			}
			intPart := sql[start:i]
			// Optional fractional part: `123.`, `123.45`. The fractional part may be empty.
			frac := ""
			hasFrac := false
			if i < len(b) && b[i] == '.' {
				hasFrac = true
				i++ // consume '.'
				fracStart := i
				for i < len(b) && isDigit(b[i]) {
					i++
				}
				frac = sql[fracStart:i]
			}
			// Optional exponent (`e3`, `E+2`, `e-10`); a well-formed exponent (even with no '.')
			// makes the literal a decimal.
			var exp int64
			var hasExp bool
			exp, hasExp, i = scanExponent(b, i)
			if hasFrac || hasExp {
				digits, scale := decimalFromParts(intPart, frac, hasExp, exp)
				tokens = append(tokens, Token{Kind: TokDecimal, Word: digits, Int: uint64(scale)})
			} else {
				// Integer literal: an unsigned magnitude. The sign is TokMinus. The magnitude
				// must be <= 2^63 so that -(2^63) = int64's minimum is reachable; anything
				// larger cannot be represented (42601). int64 cannot hold 2^63, so carry it
				// unsigned and let the parser convert.
				text := intPart
				n, err := strconv.ParseUint(text, 10, 64)
				if err != nil || n > (uint64(1)<<63) {
					return nil, NewError(SyntaxError, fmt.Sprintf("integer literal out of range: %s", text))
				}
				tokens = append(tokens, Token{Kind: TokInt, Int: n})
			}
		case c == '.':
			// A '.' has two roles, disambiguated on the FOLLOWING byte alone (no preceding-token
			// context, so the rule is trivially identical across cores — grammar.md §4): a digit
			// immediately after starts a leading-dot decimal literal (`.5`); otherwise it is the
			// TokDot of a qualified column reference (`t.col`, §15). The lone overlap — an
			// identifier then `.<digit>` (`t.5`) — is invalid either way and lexes as a decimal,
			// rejected at parse.
			if i+1 < len(b) && isDigit(b[i+1]) {
				i++ // consume '.'
				fracStart := i
				for i < len(b) && isDigit(b[i]) {
					i++
				}
				frac := sql[fracStart:i]
				// A leading-dot decimal may also carry an exponent (`.5e2`).
				var exp int64
				var hasExp bool
				exp, hasExp, i = scanExponent(b, i)
				digits, scale := decimalFromParts("", frac, hasExp, exp)
				tokens = append(tokens, Token{Kind: TokDecimal, Word: digits, Int: uint64(scale)})
			} else {
				tokens = append(tokens, Token{Kind: TokDot})
				i++
			}
		case c == '$':
			// A bind parameter $N — '$' then a 1-based decimal index (spec/design/api.md §5,
			// grammar.md §5). '$' with no following digit, $0, and a leading zero ($01) are 42601;
			// an index overflowing uint32 is too.
			i++ // consume '$'
			digitStart := i
			for i < len(b) && isDigit(b[i]) {
				i++
			}
			digits := sql[digitStart:i]
			if len(digits) == 0 {
				return nil, NewError(SyntaxError, "expected a parameter number after '$'")
			}
			if digits[0] == '0' {
				return nil, NewError(SyntaxError, fmt.Sprintf(
					"invalid parameter number $%s: parameters are 1-based with no leading zero", digits,
				))
			}
			n, err := strconv.ParseUint(digits, 10, 32)
			if err != nil {
				return nil, NewError(SyntaxError, fmt.Sprintf("parameter number out of range: $%s", digits))
			}
			tokens = append(tokens, Token{Kind: TokParam, Int: n})
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
