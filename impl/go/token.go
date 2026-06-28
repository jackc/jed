package jed

// TokenKind classifies a lexed token.
type tokenKind int

const (
	// TokWord is a bare word: keyword or identifier (compared case-insensitively).
	tokWord tokenKind = iota
	// TokInt is an integer literal's unsigned magnitude (the sign is TokMinus).
	tokInt
	// TokStr is a single-quoted string literal's decoded content (the text type): the
	// lexer strips the quotes and collapses each doubled '' to one ' (no backslash
	// escapes — standard_conforming_strings, spec/design/types.md §11).
	tokStr
	// TokQuotedIdent is a double-quoted identifier's decoded content ("en-US", "C"): the lexer
	// strips the surrounding " and collapses each doubled "" to one ". Unlike TokWord it is kept
	// VERBATIM (case-sensitive). Used for collation names (spec/design/collation.md §1); the only
	// parse position consuming one today is COLLATE "name", so it is a 42601 syntax error elsewhere.
	tokQuotedIdent
	// TokDecimal is a decimal literal (a numeric literal containing a '.'): Word holds the
	// unscaled coefficient as a decimal-digit string (leading zeros allowed, no sign) and Int
	// holds the scale (fractional digit count). 1.50 → ("150", 2). The sign is TokMinus; the
	// cap check is at resolve (spec/design/grammar.md §14).
	tokDecimal
	// TokComma is ",".
	tokComma
	// TokDot is the "." separator of a qualified column reference (t.col). Emitted only when
	// a "." is NOT part of a numeric literal — i.e. with no digit immediately after it
	// (spec/design/grammar.md §4/§15).
	tokDot
	// TokLParen is "(".
	tokLParen
	// TokRParen is ")".
	tokRParen
	// TokLBracket is "[" — the array subscript / ARRAY[…] / T[] type-suffix bracket
	// (spec/design/array.md).
	tokLBracket
	// TokRBracket is "]".
	tokRBracket
	// TokStar is "*".
	tokStar
	// TokPlus is "+".
	tokPlus
	// TokMinus is "-".
	tokMinus
	// TokSlash is "/".
	tokSlash
	// TokPercent is "%".
	tokPercent
	// TokEq is "=".
	tokEq
	// TokNe is "<>" (or its "!=" alias) — the not-equal operator. The lexer folds both
	// spellings to this one token (spec/design/grammar.md §4).
	tokNe
	// TokLt is "<".
	tokLt
	// TokGt is ">".
	tokGt
	// TokLe is "<=".
	tokLe
	// TokGe is ">=".
	tokGe
	// TokDoubleColon is the "::" PostgreSQL typecast operator (expr::type = CAST(expr AS type)).
	// Two colons, scanned greedily (spec/design/grammar.md §37).
	tokDoubleColon
	// TokColon is a single ":" — the array-slice bound separator a[m:n] (spec/design/array.md §6).
	tokColon
	// TokFatArrow is the "=>" named-argument arrow (name => value, PostgreSQL named notation).
	// Two chars, scanned greedily after "="; the legacy ":=" is not part of jed's surface
	// (spec/design/grammar.md §17).
	tokFatArrow
	// TokConcat is the "||" array concatenation operator (a || b). Two "|" scanned greedily; a lone
	// "|" is a 42601 syntax error (jed has no bitwise-or). spec/design/grammar.md §39.
	tokConcat
	// TokArrow is the "->" jsonb accessor operator (doc -> 'key' / doc -> 0). "-" then ">", scanned
	// greedily. See spec/design/json-sql-functions.md §1.
	tokArrow
	// TokArrowText is the "->>" jsonb accessor-as-text operator (doc ->> 'key'). "-" then ">>", scanned
	// greedily. See spec/design/json-sql-functions.md §1.
	tokArrowText
	// TokHashArrow is the "#>" jsonb get-at-path operator (doc #> '{a,b}'). "#" then ">", scanned
	// greedily. See spec/design/json-sql-functions.md §1.
	tokHashArrow
	// TokHashArrowText is the "#>>" jsonb get-at-path-as-text operator (doc #>> '{a,b}'). "#" then ">>",
	// scanned greedily. See spec/design/json-sql-functions.md §1.
	tokHashArrowText
	// TokQuestion is the "?" jsonb key-exists operator (doc ? 'key'). See spec/design/json-sql-functions.md §1.
	tokQuestion
	// TokQuestionPipe is the "?|" jsonb any-key-exists operator (doc ?| '{a,b}'). "?" then "|", scanned greedily.
	tokQuestionPipe
	// TokQuestionAmp is the "?&" jsonb all-keys-exist operator (doc ?& '{a,b}'). "?" then "&", scanned greedily.
	tokQuestionAmp
	// TokHashMinus is the "#-" jsonb delete-at-path operator (doc #- '{a,b}'). "#" then "-", scanned greedily.
	tokHashMinus
	// TokContains is the "@>" array containment operator (a @> b — does a contain b). "@" then ">"
	// scanned greedily; a lone "@" is a 42601 syntax error. spec/design/grammar.md §40.
	tokContains
	// TokJsonPathExists is the "@?" jsonpath-exists operator (jsonb @? jsonpath = jsonb_path_exists).
	// "@" then "?", scanned greedily. spec/design/jsonpath.md §6.
	tokJsonPathExists
	// TokJsonPathMatch is the "@@" jsonpath-match operator (jsonb @@ jsonpath = jsonb_path_match).
	// "@" then "@", scanned greedily. spec/design/jsonpath.md §6.
	tokJsonPathMatch
	// TokContainedBy is the "<@" array contained-by operator (a <@ b — is a contained by b). "<"
	// then "@". spec/design/grammar.md §40.
	tokContainedBy
	// TokOverlaps is the "&&" array overlap operator (a && b — do a and b share an element). Two "&"
	// scanned greedily; a lone "&" is a 42601 syntax error (no bitwise-and). spec/design/grammar.md §40.
	tokOverlaps
	// TokStrictlyLeft is the "<<" range strictly-left operator (a << b). Two "<". range-functions.md §3 (RF3).
	tokStrictlyLeft
	// TokStrictlyRight is the ">>" range strictly-right operator (a >> b). Two ">". range-functions.md §3 (RF3).
	tokStrictlyRight
	// TokNotExtendRight is the "&<" range not-extend-right operator (a &< b). "&" then "<". range-functions.md §3.
	tokNotExtendRight
	// TokNotExtendLeft is the "&>" range not-extend-left operator (a &> b). "&" then ">". range-functions.md §3.
	tokNotExtendLeft
	// TokAdjacent is the "-|-" range adjacency operator (a -|- b). "-" "|" "-", scanned greedily and
	// checked BEFORE the "--" line comment. range-functions.md §3 (RF3).
	tokAdjacent
	// TokTilde is the "~" regular-expression match operator (s ~ p). grammar.md §22b, regex.md.
	tokTilde
	// TokTildeStar is the "~*" case-insensitive regex match operator (s ~* p). "~" then "*", scanned
	// greedily (so "~*" is one token, never "~" TokStar). grammar.md §22b.
	tokTildeStar
	// TokBangTilde is the "!~" negated regex match operator (s !~ p). "!" then "~", checked in the
	// "!" arm BEFORE "!="→TokNe and the lone-"!" error. grammar.md §22b.
	tokBangTilde
	// TokBangTildeStar is the "!~*" negated case-insensitive regex match operator (s !~* p). "!" "~"
	// "*", scanned greedily. grammar.md §22b.
	tokBangTildeStar
	// TokParam is a bind parameter $N — Int holds the 1-based index. The lexer rejects $0, a
	// leading zero ($01), and $ with no following digit (42601). Bound by the host API, not the
	// corpus (spec/design/api.md, grammar.md §5).
	tokParam
	// TokEof marks end of input.
	tokEof
)

// Token is a lexed token. Word holds the text for TokWord; Int holds the unsigned
// magnitude for TokInt. The lexer guarantees the magnitude is <= 2^63; i64 cannot
// hold 2^63, so the parser converts — a bare magnitude > MaxInt64 traps 22003, and
// -(2^63) folds to i64's minimum (spec/design/grammar.md §4).
type token struct {
	Kind tokenKind
	Word string // TokWord, or the decoded string for TokStr
	Int  uint64
}
