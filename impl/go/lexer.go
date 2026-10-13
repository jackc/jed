package jed

import (
	"errors"
	"fmt"
	"sort"
	"strconv"
	"unicode/utf8"
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
//
// This is the stored-text entry point (a view body, a CHECK expression, a rename rewrite), so an
// error carries no position: it would point into text the host never wrote
// (spec/design/error-fields.md §10.1).
func lex(sql string) ([]token, error) {
	tokens, _, err := lexWithPositions(sql)
	if err != nil {
		var e *EngineError
		if errors.As(err, &e) {
			e.Position = 0
		}
		return nil, err
	}
	return tokens, nil
}

// lexWithPositions is lex, also returning each token's 1-based **character** position in sql
// (spec/design/error-fields.md §10.1), index-aligned with the tokens. TokEof's position is one past
// the last character — PostgreSQL's "at end of input". An error carries the position of the lexeme
// being scanned (spec/design/error-fields.md §10.2).
func lexWithPositions(sql string) ([]token, []int32, error) {
	var starts []int
	tokStart := 0
	tokens, err := lexRaw(sql, &starts, &tokStart)
	if err != nil {
		var e *EngineError
		if errors.As(err, &e) {
			e.withPosition(charPositions(sql, []int{tokStart})[0])
		}
		return nil, nil, err
	}
	return tokens, charPositions(sql, starts), nil
}

// charPositions converts ascending byte offsets into sql to 1-based character positions in one pass.
// A character is a code point: count the bytes that are not UTF-8 continuation bytes.
func charPositions(sql string, starts []int) []int32 {
	out := make([]int32, 0, len(starts))
	b, chars := 0, int32(0)
	for _, s := range starts {
		for b < s {
			if sql[b]&0xC0 != 0x80 {
				chars++
			}
			b++
		}
		out = append(out, chars+1)
	}
	return out
}

// charCount is the number of characters (code points, by the charPositions rule) in s.
func charCount(s string) int32 {
	n := int32(0)
	for i := 0; i < len(s); i++ {
		if s[i]&0xC0 != 0x80 {
			n++
		}
	}
	return n
}

// lexRaw is the lexer proper: it appends each token's start byte offset to starts, and keeps
// tokStart at the byte where the lexeme being scanned began, so the caller can position an error.
func lexRaw(sql string, starts *[]int, tokStart *int) ([]token, error) {
	b := []byte(sql)
	i := 0
	var tokens []token
	// Indices of the `U&'…'` / `U&"…"` tokens, decoded after the scan (applyUnicodeEscapes).
	var unicodeEscapes []int

	isDigit := func(c byte) bool { return c >= '0' && c <= '9' }
	isAlpha := func(c byte) bool {
		return c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
	}

	for i < len(b) {
		// The previous iteration's tokens started at its tokStart; this one's start at i.
		for len(*starts) < len(tokens) {
			*starts = append(*starts, *tokStart)
		}
		*tokStart = i
		c := b[i]
		switch {
		case c == ' ' || c == '\t' || c == '\r' || c == '\n':
			i++
		case c == ',':
			tokens = append(tokens, token{Kind: tokComma})
			i++
		case c == '(':
			tokens = append(tokens, token{Kind: tokLParen})
			i++
		case c == ')':
			tokens = append(tokens, token{Kind: tokRParen})
			i++
		case c == '[':
			tokens = append(tokens, token{Kind: tokLBracket})
			i++
		case c == ']':
			tokens = append(tokens, token{Kind: tokRBracket})
			i++
		case c == '*':
			tokens = append(tokens, token{Kind: tokStar})
			i++
		case c == '+':
			tokens = append(tokens, token{Kind: tokPlus})
			i++
		case c == '-':
			// `-|-` is the range adjacency operator (range-functions.md §3), scanned greedily and
			// checked FIRST so it is never mistaken for `-` (Minus) `|-`. Its middle `|` keeps it
			// disjoint from the `--` line comment (which needs a second `-`).
			if i+2 < len(b) && b[i+1] == '|' && b[i+2] == '-' {
				tokens = append(tokens, token{Kind: tokAdjacent})
				i += 3
			} else if i+2 < len(b) && b[i+1] == '>' && b[i+2] == '>' {
				// `->>` is the jsonb accessor-as-text operator (json-sql-functions.md §1),
				// scanned greedily BEFORE `->` so it is never `-> >`.
				tokens = append(tokens, token{Kind: tokArrowText})
				i += 3
			} else if i+1 < len(b) && b[i+1] == '>' {
				// `->` is the jsonb accessor operator (json-sql-functions.md §1).
				tokens = append(tokens, token{Kind: tokArrow})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '-' {
				// `--` starts a line comment running to the end of the line; comments are
				// whitespace (grammar.md §33). Two hyphens ALWAYS start a comment outside a
				// string, even abutting a token (`1--2` is `1` — PG behavior).
				i += 2
				for i < len(b) && b[i] != '\n' && b[i] != '\r' {
					i++
				}
			} else {
				tokens = append(tokens, token{Kind: tokMinus})
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
						return nil, newError(SyntaxError, "unterminated /* comment")
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
				tokens = append(tokens, token{Kind: tokSlash})
				i++
			}
		case c == '%':
			tokens = append(tokens, token{Kind: tokPercent})
			i++
		case c == '|':
			// `||` is the array concatenation operator (grammar.md §39), scanned greedily as one
			// token; a lone `|` is not part of jed's surface (no bitwise-or) — 42601.
			if i+1 < len(b) && b[i+1] == '|' {
				tokens = append(tokens, token{Kind: tokConcat})
				i += 2
			} else {
				return nil, newError(SyntaxError, "unexpected character '|'")
			}
		case c == ':':
			// `::` is the PostgreSQL typecast operator (grammar.md §37), scanned greedily as one
			// token; a lone `:` is the array-slice separator a[m:n] (array.md §6).
			if i+1 < len(b) && b[i+1] == ':' {
				tokens = append(tokens, token{Kind: tokDoubleColon})
				i += 2
			} else {
				tokens = append(tokens, token{Kind: tokColon})
				i++
			}
		case c == '=':
			// `=>` is the named-argument arrow (grammar.md §17), scanned greedily as one token;
			// a bare `=` is the equality operator.
			if i+1 < len(b) && b[i+1] == '>' {
				tokens = append(tokens, token{Kind: tokFatArrow})
				i += 2
			} else {
				tokens = append(tokens, token{Kind: tokEq})
				i++
			}
		case c == '<':
			if i+1 < len(b) && b[i+1] == '=' {
				tokens = append(tokens, token{Kind: tokLe})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '>' {
				// `<>` is the not-equal operator (grammar.md §4), scanned greedily; its `!=`
				// alias is handled in the `!` case and folds to the same token.
				tokens = append(tokens, token{Kind: tokNe})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '@' {
				// `<@` is the array contained-by operator (grammar.md §40), scanned greedily.
				tokens = append(tokens, token{Kind: tokContainedBy})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '<' {
				// `<<` is the range strictly-left operator (range-functions.md §3), scanned greedily.
				tokens = append(tokens, token{Kind: tokStrictlyLeft})
				i += 2
			} else {
				tokens = append(tokens, token{Kind: tokLt})
				i++
			}
		case c == '!':
			// `!~*` / `!~` are the negated regex match operators (grammar.md §22b), checked greedily
			// BEFORE the `!=`→TokNe alias and the lone-`!` error. `!=` is the PostgreSQL alias for
			// `<>`. A lone `!` is not part of jed's surface (no factorial / boolean-not) — 42601.
			if i+2 < len(b) && b[i+1] == '~' && b[i+2] == '*' {
				tokens = append(tokens, token{Kind: tokBangTildeStar})
				i += 3
			} else if i+1 < len(b) && b[i+1] == '~' {
				tokens = append(tokens, token{Kind: tokBangTilde})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '=' {
				tokens = append(tokens, token{Kind: tokNe})
				i += 2
			} else {
				return nil, newError(SyntaxError, "unexpected character '!'")
			}
		case c == '~':
			// `~*` is the case-insensitive regex match operator (grammar.md §22b), scanned greedily
			// as one token (never `~` TokStar); a bare `~` is the case-sensitive form.
			if i+1 < len(b) && b[i+1] == '*' {
				tokens = append(tokens, token{Kind: tokTildeStar})
				i += 2
			} else {
				tokens = append(tokens, token{Kind: tokTilde})
				i++
			}
		case c == '@':
			// `@>` array containment (grammar.md §40); `@?` / `@@` are the jsonpath exists / match
			// operators (jsonpath.md §6). Scanned greedily; a lone `@` is not jed's surface — 42601.
			if i+1 < len(b) && b[i+1] == '>' {
				tokens = append(tokens, token{Kind: tokContains})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '?' {
				tokens = append(tokens, token{Kind: tokJsonPathExists})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '@' {
				tokens = append(tokens, token{Kind: tokJsonPathMatch})
				i += 2
			} else {
				return nil, newError(SyntaxError, "unexpected character '@'")
			}
		case c == '#':
			// `#>>` / `#>` (get-at-path) and `#-` (delete-at-path) are the jsonb path operators
			// (json-sql-functions.md §1), scanned greedily (`#>>` before `#>`). A lone `#` is not
			// part of jed's surface — 42601.
			if i+2 < len(b) && b[i+1] == '>' && b[i+2] == '>' {
				tokens = append(tokens, token{Kind: tokHashArrowText})
				i += 3
			} else if i+1 < len(b) && b[i+1] == '>' {
				tokens = append(tokens, token{Kind: tokHashArrow})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '-' {
				tokens = append(tokens, token{Kind: tokHashMinus})
				i += 2
			} else {
				return nil, newError(SyntaxError, "unexpected character '#'")
			}
		case c == '?':
			// `?|` (any-key) / `?&` (all-keys) / `?` (key-exists) are the jsonb key-existence
			// operators (json-sql-functions.md §1), scanned greedily. A lone `?` is the key-exists
			// operator (jed binds parameters as `$N`, never `?`).
			if i+1 < len(b) && b[i+1] == '|' {
				tokens = append(tokens, token{Kind: tokQuestionPipe})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '&' {
				tokens = append(tokens, token{Kind: tokQuestionAmp})
				i += 2
			} else {
				tokens = append(tokens, token{Kind: tokQuestion})
				i++
			}
		case c == '&':
			// `&&` is the array overlap operator (grammar.md §40); `&<` (not-extend-right) and `&>`
			// (not-extend-left) are the range positional operators (range-functions.md §3). Each
			// scanned greedily; a lone `&` is not part of jed's surface (no bitwise-and) — 42601.
			if i+1 < len(b) && b[i+1] == '&' {
				tokens = append(tokens, token{Kind: tokOverlaps})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '<' {
				tokens = append(tokens, token{Kind: tokNotExtendRight})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '>' {
				tokens = append(tokens, token{Kind: tokNotExtendLeft})
				i += 2
			} else {
				return nil, newError(SyntaxError, "unexpected character '&'")
			}
		case c == '>':
			if i+1 < len(b) && b[i+1] == '=' {
				tokens = append(tokens, token{Kind: tokGe})
				i += 2
			} else if i+1 < len(b) && b[i+1] == '>' {
				// `>>` is the range strictly-right operator (range-functions.md §3), scanned greedily.
				tokens = append(tokens, token{Kind: tokStrictlyRight})
				i += 2
			} else {
				tokens = append(tokens, token{Kind: tokGt})
				i++
			}
		case c == '\'':
			// Single-quoted string literal (the text type). `''` is an embedded single
			// quote; backslash is an ordinary character (no C-style escapes —
			// standard_conforming_strings, spec/design/types.md §11). The input is valid
			// UTF-8 and `'` is ASCII (never a UTF-8 continuation byte), so copying raw bytes
			// between quotes preserves UTF-8 validity.
			i++ // consume the opening quote
			sb, closed := scanQuotedBody(b, &i, '\'')
			if !closed {
				return nil, newError(SyntaxError, "unterminated string literal")
			}
			tokens = append(tokens, token{Kind: tokStr, Word: string(sb)})
		case c == '"':
			// Double-quoted identifier (spec/design/grammar.md §3): the name VERBATIM (case-sensitive,
			// any UTF-8), `""` an embedded double quote. `"` is ASCII, so copying raw bytes between
			// quotes preserves UTF-8 validity.
			i++ // consume the opening quote
			sb, closed := scanQuotedBody(b, &i, '"')
			if !closed {
				return nil, newError(SyntaxError, "unterminated quoted identifier")
			}
			// A zero-length delimited identifier is a syntax error (PostgreSQL's rule,
			// spec/design/grammar.md §3).
			if len(sb) == 0 {
				return nil, newError(SyntaxError, "zero-length delimited identifier")
			}
			// The identifier-length gate applies to a quoted name too, measured on its decoded
			// UTF-8 bytes (spec/design/cost.md §7).
			if len(sb) > maxIdentifierLength {
				return nil, nameTooLongError()
			}
			tokens = append(tokens, token{Kind: tokQuotedIdent, Word: string(sb)})
		case (c == 'u' || c == 'U') && i+2 < len(b) && b[i+1] == '&' && (b[i+2] == '\'' || b[i+2] == '"'):
			// A Unicode-escape string `U&'…'` or identifier `U&"…"` (grammar.md §3). The body is
			// scanned exactly like its plain form (`''` / `""` doubled) and kept RAW here;
			// applyUnicodeEscapes decodes it once the whole input is lexed, because an optional
			// `UESCAPE '<c>'` after it chooses the escape character.
			quote := b[i+2]
			i += 3 // consume `U&` and the opening quote
			sb, closed := scanQuotedBody(b, &i, quote)
			if !closed {
				if quote == '\'' {
					return nil, newError(SyntaxError, "unterminated string literal")
				}
				return nil, newError(SyntaxError, "unterminated quoted identifier")
			}
			// The zero-length check precedes decoding (PostgreSQL's order); every escape decodes
			// to at least one character, so a non-empty body never decodes empty.
			if quote == '"' && len(sb) == 0 {
				return nil, newError(SyntaxError, "zero-length delimited identifier")
			}
			unicodeEscapes = append(unicodeEscapes, len(tokens))
			if quote == '\'' {
				tokens = append(tokens, token{Kind: tokStr, Word: string(sb)})
			} else {
				tokens = append(tokens, token{Kind: tokQuotedIdent, Word: string(sb)})
			}
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
				tokens = append(tokens, token{Kind: tokDecimal, Word: digits, Int: uint64(scale)})
			} else {
				// Integer literal: an unsigned magnitude. The sign is TokMinus. The magnitude
				// must be <= 2^63 so that -(2^63) = i64's minimum is reachable; anything
				// larger cannot be represented (42601). i64 cannot hold 2^63, so carry it
				// unsigned and let the parser convert.
				text := intPart
				n, err := strconv.ParseUint(text, 10, 64)
				if err != nil || n > (uint64(1)<<63) {
					return nil, newError(SyntaxError, fmt.Sprintf("integer literal out of range: %s", text))
				}
				tokens = append(tokens, token{Kind: tokInt, Int: n})
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
				tokens = append(tokens, token{Kind: tokDecimal, Word: digits, Int: uint64(scale)})
			} else {
				tokens = append(tokens, token{Kind: tokDot})
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
				return nil, newError(SyntaxError, "expected a parameter number after '$'")
			}
			if digits[0] == '0' {
				return nil, newError(SyntaxError, fmt.Sprintf(
					"invalid parameter number $%s: parameters are 1-based with no leading zero", digits,
				))
			}
			n, err := strconv.ParseUint(digits, 10, 32)
			if err != nil {
				return nil, newError(SyntaxError, fmt.Sprintf("parameter number out of range: $%s", digits))
			}
			tokens = append(tokens, token{Kind: tokParam, Int: n})
		case isAlpha(c):
			start := i
			for i < len(b) && (isAlpha(b[i]) || isDigit(b[i])) {
				i++
			}
			// Identifier-length gate (CLAUDE.md §13; spec/design/cost.md §7). A word is an
			// identifier or a keyword; identifiers are ASCII-only here (so bytes = chars), and no
			// keyword is this long, so bounding the word length bounds every identifier on every
			// parse path. Aborts with 42622 before the (possibly huge) name is interned.
			if i-start > maxIdentifierLength {
				return nil, nameTooLongError()
			}
			// A bare word folds to lowercase (ASCII) here, so every later comparison of a name is
			// byte-exact and keyword matching is case-insensitive by construction
			// (spec/design/grammar.md §3).
			tokens = append(tokens, token{Kind: tokWord, Word: asciiLower(sql[start:i])})
		default:
			return nil, newError(SyntaxError, fmt.Sprintf("unexpected character '%c'", c))
		}
	}

	for len(*starts) < len(tokens) {
		*starts = append(*starts, *tokStart)
	}
	*tokStart = len(b)
	tokens = append(tokens, token{Kind: tokEof})
	*starts = append(*starts, len(b))
	if len(unicodeEscapes) > 0 {
		return applyUnicodeEscapes(tokens, starts, unicodeEscapes, tokStart)
	}
	return tokens, nil
}

// scanQuotedBody scans a quoted body from b[*i:] (just past the opening quote) through its closing
// quote, collapsing each doubled quote to one. closed is false when the input ends first.
func scanQuotedBody(b []byte, i *int, quote byte) (body []byte, closed bool) {
	for *i < len(b) {
		if b[*i] == quote {
			if *i+1 < len(b) && b[*i+1] == quote {
				body = append(body, quote)
				*i += 2
				continue
			}
			*i++ // consume the closing quote
			return body, true
		}
		body = append(body, b[*i])
		*i++
	}
	return body, false
}

// applyUnicodeEscapes decodes every `U&'…'` / `U&"…"` token (their indices in uesc, ascending) —
// PostgreSQL's base_yylex filter and str_udeescape (grammar.md §3). A `UESCAPE '<c>'` right after
// the token picks the escape character and is consumed with it; otherwise the escape is `\`. The
// decoded token is a plain tokStr / tokQuotedIdent, so nothing downstream (and no stored text, which
// renders it with plain quotes) ever sees the Unicode-escape spelling. An error sets *tokStart to
// the byte PostgreSQL's error cursor names.
func applyUnicodeEscapes(tokens []token, starts *[]int, uesc []int, tokStart *int) ([]token, error) {
	isUesc := func(idx int) bool {
		k := sort.SearchInts(uesc, idx)
		return k < len(uesc) && uesc[k] == idx
	}
	out := make([]token, 0, len(tokens))
	outStarts := make([]int, 0, len(tokens))
	for idx := 0; idx < len(tokens); {
		if !isUesc(idx) {
			out = append(out, tokens[idx])
			outStarts = append(outStarts, (*starts)[idx])
			idx++
			continue
		}
		start := (*starts)[idx]
		escape := byte('\\')
		next := idx + 1
		if tokens[idx+1].Kind == tokWord && tokens[idx+1].Word == "uescape" {
			// The third token must be a simple string literal of exactly one byte that is not a
			// hex digit, `+`, `'`, `"`, or whitespace. Errors point at that third token. tokEof
			// always follows, so idx+2 is in range.
			third := idx + 2
			*tokStart = (*starts)[third]
			if tokens[third].Kind != tokStr || isUesc(third) {
				return nil, newError(SyntaxError, "UESCAPE must be followed by a simple string literal")
			}
			esc := tokens[third].Word
			if len(esc) != 1 || !isUescapeChar(esc[0]) {
				return nil, newError(SyntaxError, "invalid Unicode escape character")
			}
			escape = esc[0]
			next = idx + 3
		}
		isIdent := tokens[idx].Kind == tokQuotedIdent
		// PostgreSQL's cursor is the token start + 3 (`U&'`) + the offset into the body AFTER `''`
		// / `""` collapsing, measured in bytes.
		decoded, off, err := udeescape(tokens[idx].Word, escape)
		if err != nil {
			*tokStart = start + 3 + off
			return nil, err
		}
		if isIdent && len(decoded) > maxIdentifierLength {
			*tokStart = start
			return nil, nameTooLongError()
		}
		out = append(out, token{Kind: tokens[idx].Kind, Word: decoded})
		outStarts = append(outStarts, start)
		idx = next
	}
	*starts = outStarts
	return out, nil
}

// isUescapeChar is PostgreSQL's check_uescapechar: a UESCAPE character may not be a hex digit, `+`,
// `'`, `"`, or scanner whitespace.
func isUescapeChar(c byte) bool {
	switch {
	case isHexDigit(c), c == '+', c == '\'', c == '"':
		return false
	case c == ' ', c == '\t', c == '\n', c == '\r', c == 0x0B, c == 0x0C:
		return false
	}
	return true
}

func isHexDigit(c byte) bool {
	return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F')
}

// hexDigits is the value of n hex digits at raw[at:], or ok=false if any is missing or not a hex
// digit.
func hexDigits(raw string, at, n int) (uint32, bool) {
	if at+n > len(raw) {
		return 0, false
	}
	var v uint32
	for k := at; k < at+n; k++ {
		c := raw[k]
		var d byte
		switch {
		case c >= '0' && c <= '9':
			d = c - '0'
		case c >= 'a' && c <= 'f':
			d = c - 'a' + 10
		case c >= 'A' && c <= 'F':
			d = c - 'A' + 10
		default:
			return 0, false
		}
		v = v<<4 | uint32(d)
	}
	return v, true
}

// udeescape is PostgreSQL's str_udeescape: replace `<esc>XXXX` / `<esc>+XXXXXX` with the code
// point's UTF-8 and `<esc><esc>` with `<esc>`, combining a UTF-16 surrogate pair written as two
// escapes. An error returns the byte offset into raw that PostgreSQL's cursor names (the escape, or
// the byte after a dangling high surrogate).
func udeescape(raw string, esc byte) (string, int, error) {
	invalidPair := func() error { return newError(SyntaxError, "invalid Unicode surrogate pair") }
	out := make([]byte, 0, len(raw))
	var pairFirst uint32
	i := 0
	for i < len(raw) {
		if raw[i] != esc {
			if pairFirst != 0 {
				return "", i, invalidPair()
			}
			out = append(out, raw[i])
			i++
			continue
		}
		if i+1 < len(raw) && raw[i+1] == esc {
			if pairFirst != 0 {
				return "", i, invalidPair()
			}
			out = append(out, esc)
			i += 2
			continue
		}
		var cp uint32
		var n int
		if v, ok := hexDigits(raw, i+1, 4); ok {
			cp, n = v, 5
		} else if v, ok := hexDigits(raw, i+2, 6); ok && i+1 < len(raw) && raw[i+1] == '+' {
			cp, n = v, 8
		} else {
			return "", i, newError(SyntaxError, "invalid Unicode escape").
				withHint(`Unicode escapes must be \XXXX or \+XXXXXX.`)
		}
		if cp == 0 || cp > 0x10FFFF {
			return "", i, newError(SyntaxError, "invalid Unicode escape value")
		}
		isHigh := cp >= 0xD800 && cp <= 0xDBFF
		isLow := cp >= 0xDC00 && cp <= 0xDFFF
		if pairFirst != 0 {
			if !isLow {
				return "", i, invalidPair()
			}
			cp = ((pairFirst&0x3FF)<<10 | cp&0x3FF) + 0x10000
			pairFirst = 0
		} else if isLow {
			return "", i, invalidPair()
		} else if isHigh {
			pairFirst = cp
			i += n
			continue
		}
		out = utf8.AppendRune(out, rune(cp))
		i += n
	}
	if pairFirst != 0 {
		return "", len(raw), invalidPair()
	}
	return string(out), 0, nil
}

func nameTooLongError() *EngineError {
	return newError(NameTooLong, fmt.Sprintf("identifier exceeds the maximum length of %d bytes", maxIdentifierLength))
}

// asciiLower folds ASCII A-Z to a-z and leaves every other byte unchanged — the bare-identifier
// fold (spec/design/grammar.md §3). Returns s itself when it has no uppercase byte.
func asciiLower(s string) string {
	for i := 0; i < len(s); i++ {
		if s[i] >= 'A' && s[i] <= 'Z' {
			b := []byte(s)
			for j := i; j < len(b); j++ {
				if b[j] >= 'A' && b[j] <= 'Z' {
					b[j] += 'a' - 'A'
				}
			}
			return string(b)
		}
	}
	return s
}
