package jed

// JSON document types (spec/design/json.md): `json` (validated, stored verbatim as text) and
// `jsonb` (parsed, canonicalized, stored as a compact tagged-node tree). Numbers are exact
// `Decimal` (PG `numeric`, never binary float — CLAUDE.md §8); strings are UTF-8 `text`; `jsonb`
// objects keep their keys in a canonical sorted order (length-then-bytewise) with duplicates
// resolved last-wins, so the in-memory tree and the on-disk bytes are a pure function of the value
// (no hashmap-iteration-order leak — §2.3).
//
// Hand-written per CLAUDE.md §5 (a recursive tree codec/comparator/parser is irreducibly
// per-language), cross-checked across cores by the conformance corpus + golden fixtures.

import (
	"fmt"
	"math"
	"strconv"
	"strings"
	"unicode/utf8"
)

// JsonNodeKind tags a jsonb node (spec/design/json.md §2).
type JsonNodeKind int

const (
	// JNull is JSON `null` — a concrete node, wholly distinct from a SQL NULL `jsonb` value.
	JNull JsonNodeKind = iota
	// JBool is a JSON boolean (B holds the value).
	JBool
	// JNumber is a JSON number, held EXACTLY as a Decimal (PG numeric); no binary float ever appears.
	JNumber
	// JString is a JSON string (S holds the decoded UTF-8 content).
	JString
	// JArray is a JSON array (Arr holds its elements in order).
	JArray
	// JObject is a JSON object. For a jsonb node Obj is in canonical key order with unique keys (the
	// canonicalizer's invariant); a `json`-on-demand parse (§4) keeps input order + dupes.
	JObject
)

// JsonNode is a jsonb node — the in-memory canonical tree (spec/design/json.md §2). Object members
// are kept in canonical key order (shorter key first, ties bytewise) with duplicates removed
// (last-wins), so structural equality (via the value key / Cmp == 0) IS the correct value-level
// equality (§5). JSON `null` is the concrete JNull node, wholly distinct from a SQL NULL `jsonb`
// value. Go has no sum types, so the kind discriminant selects which field is meaningful (the same
// idiom Value uses). A JsonNode is held by pointer in Value so the flat struct stays comparable.
type JsonNode struct {
	Kind JsonNodeKind
	B    bool         // JBool
	Num  Decimal      // JNumber
	S    string       // JString
	Arr  []JsonNode   // JArray
	Obj  []JsonMember // JObject
}

// JsonMember is one object member: a key string and its value node.
type JsonMember struct {
	Key string
	Val JsonNode
}

// jsonTypeRank is the PG jsonb type-rank discriminator (spec/design/json.md §5): the outermost
// ordering key. Object > Array > Boolean > Number > String > Null.
func jsonTypeRank(n *JsonNode) int {
	switch n.Kind {
	case JNull:
		return 0
	case JString:
		return 1
	case JNumber:
		return 2
	case JBool:
		return 3
	case JArray:
		return 4
	default: // JObject
		return 5
	}
}

// Cmp is the PG jsonb total btree order (spec/design/json.md §5). A definite ordering (no SQL NULLs
// inside a document), driving both `<` and `ORDER BY` from one comparator so they agree by
// construction. Type rank first; within a type: booleans false<true, numbers by Decimal value,
// strings by collation-`C` UTF-8 byte order, arrays/objects by element/member COUNT first (PG
// compares container length before contents) then element-wise.
func (n *JsonNode) Cmp(other *JsonNode) int {
	ra, rb := jsonTypeRank(n), jsonTypeRank(other)
	if ra != rb {
		return cmpInt(ra, rb)
	}
	switch n.Kind {
	case JNull:
		return 0
	case JBool:
		// false < true.
		return cmpInt(boolRank(n.B), boolRank(other.B))
	case JNumber:
		return n.Num.CmpValue(other.Num)
	case JString:
		return strings.Compare(n.S, other.S)
	case JArray:
		if c := cmpInt(len(n.Arr), len(other.Arr)); c != 0 {
			return c
		}
		for i := range n.Arr {
			if c := n.Arr[i].Cmp(&other.Arr[i]); c != 0 {
				return c
			}
		}
		return 0
	default: // JObject
		if c := cmpInt(len(n.Obj), len(other.Obj)); c != 0 {
			return c
		}
		// Members are in canonical key order in both; compare keys then values pairwise.
		for i := range n.Obj {
			if c := jsonKeyCmp(n.Obj[i].Key, other.Obj[i].Key); c != 0 {
				return c
			}
			if c := n.Obj[i].Val.Cmp(&other.Obj[i].Val); c != 0 {
				return c
			}
		}
		return 0
	}
}

func boolRank(b bool) int {
	if b {
		return 1
	}
	return 0
}

// jsonKeyCmp is the canonical object-key order (spec/design/json.md §2.3): shorter key first, ties
// broken bytewise — PostgreSQL's jsonb key order. The canonicalizer sorts by this and the comparator
// compares keys by this.
func jsonKeyCmp(a, b string) int {
	if c := cmpInt(len(a), len(b)); c != 0 {
		return c
	}
	return strings.Compare(a, b)
}

// jsonbValueEqual reports whether two jsonb nodes are value-equal (Cmp == 0). Because the canonical
// form makes structural equality the value equality (§5), this drives `=` (always definite — no SQL
// NULLs inside a document, PG btree not 3VL).
func jsonbValueEqual(a, b *JsonNode) bool { return a.Cmp(b) == 0 }

// ---------------------------------------------------------------------------------------------
// Parsing (RFC 8259). `jsonbIn` canonicalizes; `validateJSON` validates and the caller stores
// verbatim.
// ---------------------------------------------------------------------------------------------

// maxJSONDepth is the maximum nesting depth of a `json`/`jsonb` document (spec/design/json.md §6.4):
// the largest number of containers (arrays/objects) on any root-to-leaf path — a scalar is depth 0,
// `[]` depth 1. Every producer (the text parser, the constructors, the on-disk decoder) enforces it,
// so every recursive walk over a node (compare, render, codec) is bounded by it. A fixed, cross-core
// constant sized for the weakest core's native stack; also bounds jsonpath nesting.
const maxJSONDepth = 256

// jsonTooDeep is `54001` statement_too_complex — a document (or constructor result) deeper than
// maxJSONDepth (spec/design/json.md §6.4).
func jsonTooDeep() error {
	return newError(StatementTooComplex,
		"json nesting depth exceeds the maximum of "+strconv.Itoa(maxJSONDepth))
}

// checkJSONDepth enforces maxJSONDepth on a constructed `jsonb` node — the constructor gate
// (json.md §6.4): an operation that nests existing values inside a new container checks its result.
// The walk stops one level past the limit, so it never recurses deeper than maxJSONDepth + 1.
func checkJSONDepth(node *JsonNode) error {
	if jsonExceedsDepth(node, maxJSONDepth) {
		return jsonTooDeep()
	}
	return nil
}

// jsonExceedsDepth reports whether node nests more than budget containers deep.
func jsonExceedsDepth(node *JsonNode, budget int) bool {
	switch node.Kind {
	case JArray:
		if budget == 0 {
			return true
		}
		for i := range node.Arr {
			if jsonExceedsDepth(&node.Arr[i], budget-1) {
				return true
			}
		}
	case JObject:
		if budget == 0 {
			return true
		}
		for i := range node.Obj {
			if jsonExceedsDepth(&node.Obj[i].Val, budget-1) {
				return true
			}
		}
	}
	return false
}

// checkJSONTextDepth enforces maxJSONDepth on a constructed textual `json` value (json.md §6.4) —
// a NON-recursive bracket scan of well-formed JSON text (brackets inside strings are skipped).
func checkJSONTextDepth(text string) error {
	depth := 0
	inString, escaped := false, false
	for i := 0; i < len(text); i++ {
		b := text[i]
		if inString {
			if escaped {
				escaped = false
			} else if b == '\\' {
				escaped = true
			} else if b == '"' {
				inString = false
			}
			continue
		}
		switch b {
		case '"':
			inString = true
		case '[', '{':
			depth++
			if depth > maxJSONDepth {
				return jsonTooDeep()
			}
		case ']', '}':
			if depth > 0 {
				depth--
			}
		}
	}
	return nil
}

func jsonMalformed(detail string) error {
	return newError(InvalidTextRepresentation, "invalid input syntax for type json: "+detail)
}

// jsonEscapeSyntaxError is PostgreSQL's 22P02 `invalid input syntax for type {typeName}` carrying one
// of its json_errdetail / jsonpath-scanner sentences as the Detail (error-fields.md §9.2) — the
// `\u`-escape family, whose text jed matches byte-for-byte.
func jsonEscapeSyntaxError(typeName, detail string) error {
	return newError(InvalidTextRepresentation, "invalid input syntax for type "+typeName).withDetail(detail)
}

// jsonEscapeFormatError is a `\u` escape not followed by four hex digits (PG
// JSON_UNICODE_ESCAPE_FORMAT).
func jsonEscapeFormatError() error {
	return jsonEscapeSyntaxError("json", `"\u" must be followed by four hexadecimal digits.`)
}

// jsonHighSurrogateDetail is the Detail of a high surrogate followed by another high surrogate (PG
// JSON_UNICODE_HIGH_SURROGATE, and the jsonpath scanner's same sentence).
const jsonHighSurrogateDetail = "Unicode high surrogate must not follow a high surrogate."

// jsonLowSurrogateDetail is the Detail of a low surrogate without a preceding high one, or a high
// surrogate not followed by a low one (PG JSON_UNICODE_LOW_SURROGATE, and the jsonpath scanner's).
const jsonLowSurrogateDetail = "Unicode low surrogate must follow a high surrogate."

// jsonUnicodeZeroError is 22P05 for a decoded `\u0000` (PG JSON_UNICODE_CODE_POINT_ZERO; the jsonpath
// scanner raises the same): text cannot hold the NUL character (json.md §6.2).
func jsonUnicodeZeroError() error {
	return newError(UntranslatableCharacter, "unsupported Unicode escape sequence").
		withDetail(`\u0000 cannot be converted to text.`)
}

// jsonbIn parses + canonicalizes JSON text into a jsonb node tree (`jsonb_in` — spec/design/json.md
// §6.2): numbers → Decimal, object keys deduped last-wins then sorted length-then-bytewise.
// Malformed input → 22P02; a `\u0000` escape → 22P05.
func jsonbIn(input string) (JsonNode, error) {
	p := &jsonParser{buf: []byte(input), canonicalize: true, needEscapes: true}
	return p.parseDocument()
}

// validateJSON validates JSON text well-formedness (`json_in` — spec/design/json.md §4); the caller
// stores the original bytes verbatim. Malformed input → 22P02. Like PostgreSQL's json_in this only
// lexes: a `\u` escape must have four hex digits, but `\u0000` and an unpaired surrogate are accepted
// (they fail later, in a function that de-escapes the string — json.md §4.2).
func validateJSON(input string) error {
	p := &jsonParser{buf: []byte(input)}
	_, err := p.parseDocument()
	return err
}

// parsePreservingJSON parses JSON text into a node tree WITHOUT canonicalizing (object key order +
// duplicates preserved) — the on-demand structural parse a `json` operator needs
// (spec/design/json.md §4). Strings are de-escaped, so a `\u0000` (22P05) or an unpaired surrogate
// (22P02) anywhere in the document raises (PG's need_escapes parse, json.md §4.2).
func parsePreservingJSON(input string) (JsonNode, error) {
	p := &jsonParser{buf: []byte(input), needEscapes: true}
	return p.parseDocument()
}

// parseShapeJSON is the node SHAPE of JSON text, for a consumer that never reads string contents
// (json_typeof, json_array_length, IS JSON without UNIQUE KEYS — PG lexes these without
// need_escapes): every string (member names included) is left EMPTY, and `\u` escapes are only
// lexed, never decoded or checked (json.md §4.2).
func parseShapeJSON(input string) (JsonNode, error) {
	p := &jsonParser{buf: []byte(input)}
	return p.parseDocument()
}

// parsePreservingJSONUnlessRoot is the node tree of a `json` argument to a de-escaping function that
// raises a kind error on a root container of the wrong kind (wrongRoot is '[' or '{'). PostgreSQL
// raises that error at the root's opening bracket, before lexing anything inside it, so such a root
// is returned as its parseShapeJSON (the caller raises the kind error and reads no string); any other
// root is parsePreservingJSON'd, its escapes checked first (json.md §4.2).
func parsePreservingJSONUnlessRoot(input string, wrongRoot byte) (JsonNode, error) {
	if start := skipJSONWS(input, 0); start < len(input) && input[start] == wrongRoot {
		return parseShapeJSON(input)
	}
	return parsePreservingJSON(input)
}

// checkJSONEscapes checks the escapes of every string token of well-formed JSON text the way a
// de-escaping parse would (PG lexes a `json` argument with need_escapes for every function that reads
// string contents): the first `\u0000` (22P05) or unpaired surrogate (22P02) in document order raises
// (json.md §4.2).
func checkJSONEscapes(text string) error {
	// Only a `\u` escape can fail the check, so a document without one skips the scan.
	if !strings.Contains(text, `\u`) {
		return nil
	}
	p := &jsonParser{buf: []byte(text), needEscapes: true}
	pos := 0
	for pos < len(text) {
		if text[pos] == '"' {
			p.pos = pos
			if _, err := p.parseString(); err != nil {
				return err
			}
			pos = p.pos
		} else {
			pos++
		}
	}
	return nil
}

type jsonParser struct {
	buf []byte
	pos int
	// canonicalize: when true (jsonb), objects dedup last-wins and sort keys; when false (json
	// validation / on-demand parse), members are kept in input order with duplicates.
	canonicalize bool
	// needEscapes is PostgreSQL's need_escapes (json.md §4.2): when true, strings are de-escaped —
	// surrogate pairs combined, and `\u0000` (22P05) or an unpaired surrogate (22P02) rejected; when
	// false (json_in, parseShapeJSON), escapes are only lexed and strings are left empty.
	needEscapes bool
	// depth is the current container nesting depth (spec/design/json.md §6.4): entering the
	// maxJSONDepth+1-th `[`/`{` aborts 54001 before parsing anything inside it.
	depth int
}

// descend enters one container level, enforcing maxJSONDepth (json.md §6.4).
func (p *jsonParser) descend() error {
	p.depth++
	if p.depth > maxJSONDepth {
		return jsonTooDeep()
	}
	return nil
}

// parseDocument parses a full JSON document: one value, surrounded by optional whitespace, nothing
// trailing.
func (p *jsonParser) parseDocument() (JsonNode, error) {
	p.skipWS()
	node, err := p.parseValue()
	if err != nil {
		return JsonNode{}, err
	}
	p.skipWS()
	if p.pos != len(p.buf) {
		return JsonNode{}, jsonMalformed("trailing characters after JSON value")
	}
	return node, nil
}

func (p *jsonParser) skipWS() {
	for p.pos < len(p.buf) {
		switch p.buf[p.pos] {
		case ' ', '\t', '\n', '\r':
			p.pos++
		default:
			return
		}
	}
}

// peek returns the current byte and ok (false at end of input).
func (p *jsonParser) peek() (byte, bool) {
	if p.pos < len(p.buf) {
		return p.buf[p.pos], true
	}
	return 0, false
}

func (p *jsonParser) parseValue() (JsonNode, error) {
	c, ok := p.peek()
	if !ok {
		return JsonNode{}, jsonMalformed("unexpected end of input")
	}
	switch {
	case c == '{':
		if err := p.descend(); err != nil {
			return JsonNode{}, err
		}
		node, err := p.parseObject()
		p.depth--
		return node, err
	case c == '[':
		if err := p.descend(); err != nil {
			return JsonNode{}, err
		}
		node, err := p.parseArray()
		p.depth--
		return node, err
	case c == '"':
		s, err := p.parseString()
		if err != nil {
			return JsonNode{}, err
		}
		return JsonNode{Kind: JString, S: s}, nil
	case c == 't':
		if err := p.expectKeyword("true"); err != nil {
			return JsonNode{}, err
		}
		return JsonNode{Kind: JBool, B: true}, nil
	case c == 'f':
		if err := p.expectKeyword("false"); err != nil {
			return JsonNode{}, err
		}
		return JsonNode{Kind: JBool, B: false}, nil
	case c == 'n':
		if err := p.expectKeyword("null"); err != nil {
			return JsonNode{}, err
		}
		return JsonNode{Kind: JNull}, nil
	case c == '-' || (c >= '0' && c <= '9'):
		return p.parseNumber()
	default:
		return JsonNode{}, jsonMalformed("unexpected character '" + string(rune(c)) + "'")
	}
}

func (p *jsonParser) expectKeyword(kw string) error {
	end := p.pos + len(kw)
	if end <= len(p.buf) && string(p.buf[p.pos:end]) == kw {
		p.pos = end
		return nil
	}
	return jsonMalformed("expected '" + kw + "'")
}

func (p *jsonParser) parseObject() (JsonNode, error) {
	p.pos++ // consume '{'
	var members []JsonMember
	p.skipWS()
	if c, ok := p.peek(); ok && c == '}' {
		p.pos++
		return JsonNode{Kind: JObject, Obj: members}, nil
	}
	for {
		p.skipWS()
		if c, ok := p.peek(); !ok || c != '"' {
			return JsonNode{}, jsonMalformed("expected string key in object")
		}
		key, err := p.parseString()
		if err != nil {
			return JsonNode{}, err
		}
		p.skipWS()
		if c, ok := p.peek(); !ok || c != ':' {
			return JsonNode{}, jsonMalformed("expected ':' after object key")
		}
		p.pos++
		p.skipWS()
		val, err := p.parseValue()
		if err != nil {
			return JsonNode{}, err
		}
		members = append(members, JsonMember{Key: key, Val: val})
		p.skipWS()
		c, ok := p.peek()
		switch {
		case ok && c == ',':
			p.pos++
		case ok && c == '}':
			p.pos++
			goto done
		default:
			return JsonNode{}, jsonMalformed("expected ',' or '}' in object")
		}
	}
done:
	if p.canonicalize {
		members = canonicalizeObject(members)
	}
	return JsonNode{Kind: JObject, Obj: members}, nil
}

func (p *jsonParser) parseArray() (JsonNode, error) {
	p.pos++ // consume '['
	var elems []JsonNode
	p.skipWS()
	if c, ok := p.peek(); ok && c == ']' {
		p.pos++
		return JsonNode{Kind: JArray, Arr: elems}, nil
	}
	for {
		p.skipWS()
		val, err := p.parseValue()
		if err != nil {
			return JsonNode{}, err
		}
		elems = append(elems, val)
		p.skipWS()
		c, ok := p.peek()
		switch {
		case ok && c == ',':
			p.pos++
		case ok && c == ']':
			p.pos++
			return JsonNode{Kind: JArray, Arr: elems}, nil
		default:
			return JsonNode{}, jsonMalformed("expected ',' or ']' in array")
		}
	}
}

// parseString parses a JSON string token (the leading `"` is at p.pos) — a port of PostgreSQL's
// json_lex_string (src/common/jsonapi.c). RFC 8259: `\" \\ \/ \b \f \n \r \t` and `\uXXXX`; unescaped
// control characters (< 0x20) are rejected. With needEscapes the content is decoded: a high surrogate
// must be followed directly by a `\u` low surrogate (the pair combines), anything else after it — and
// a low surrogate without one — is 22P02 with PG's Detail, and a decoded `\u0000` is 22P05. Without it
// the content is not built (an empty string is returned).
func (p *jsonParser) parseString() (string, error) {
	p.pos++ // consume opening '"'
	var out strings.Builder
	// hiSurrogate is a pending high surrogate awaiting its low half (PG's hi_surrogate); -1 = none.
	hiSurrogate := -1
	for {
		c, ok := p.peek()
		if !ok {
			return "", jsonMalformed("unterminated string")
		}
		switch {
		case c == '"':
			if hiSurrogate != -1 {
				return "", jsonEscapeSyntaxError("json", jsonLowSurrogateDetail)
			}
			p.pos++
			return out.String(), nil
		case c == '\\':
			p.pos++
			e, ok := p.peek()
			if !ok {
				return "", jsonMalformed("unterminated escape")
			}
			if e == 'u' {
				p.pos++
				ch, err := p.parseHex4()
				if err != nil {
					return "", err
				}
				if !p.needEscapes {
					continue
				}
				if ch >= 0xD800 && ch <= 0xDBFF {
					if hiSurrogate != -1 {
						return "", jsonEscapeSyntaxError("json", jsonHighSurrogateDetail)
					}
					hiSurrogate = ch
					continue
				} else if ch >= 0xDC00 && ch <= 0xDFFF {
					if hiSurrogate == -1 {
						return "", jsonEscapeSyntaxError("json", jsonLowSurrogateDetail)
					}
					ch = 0x10000 + (((hiSurrogate - 0xD800) << 10) | (ch - 0xDC00))
					hiSurrogate = -1
				}
				if hiSurrogate != -1 {
					return "", jsonEscapeSyntaxError("json", jsonLowSurrogateDetail)
				}
				if ch == 0 {
					return "", jsonUnicodeZeroError()
				}
				out.WriteRune(rune(ch))
				continue // parseHex4 already advanced pos past the 4 digits
			}
			if hiSurrogate != -1 {
				return "", jsonEscapeSyntaxError("json", jsonLowSurrogateDetail)
			}
			var decoded byte
			switch e {
			case '"', '\\', '/':
				decoded = e
			case 'b':
				decoded = '\b'
			case 'f':
				decoded = '\f'
			case 'n':
				decoded = '\n'
			case 'r':
				decoded = '\r'
			case 't':
				decoded = '\t'
			default:
				return "", jsonMalformed("invalid escape sequence")
			}
			if p.needEscapes {
				out.WriteByte(decoded)
			}
			p.pos++
		case hiSurrogate != -1:
			return "", jsonEscapeSyntaxError("json", jsonLowSurrogateDetail)
		case c <= 0x1F:
			return "", jsonMalformed("control character in string must be escaped")
		default:
			// Copy one UTF-8 code point verbatim. Determine its byte length.
			n := utf8Len(c)
			end := p.pos + n
			if end > len(p.buf) {
				return "", jsonMalformed("truncated UTF-8 sequence in string")
			}
			r, size := utf8.DecodeRune(p.buf[p.pos:end])
			if r == utf8.RuneError && size <= 1 {
				return "", jsonMalformed("invalid UTF-8 in string")
			}
			if p.needEscapes {
				out.Write(p.buf[p.pos:end])
			}
			p.pos = end
		}
	}
}

// parseHex4 reads exactly four hex digits as a code-unit (the cursor is just past `\u`). A non-hex
// digit is PG's JSON_UNICODE_ESCAPE_FORMAT (22P02 with its Detail); running out of input first is a
// truncated token.
func (p *jsonParser) parseHex4() (int, error) {
	v := 0
	for i := 0; i < 4; i++ {
		if p.pos+i >= len(p.buf) {
			return 0, jsonMalformed("truncated \\u escape")
		}
		d := p.buf[p.pos+i]
		var nib int
		switch {
		case d >= '0' && d <= '9':
			nib = int(d - '0')
		case d >= 'a' && d <= 'f':
			nib = int(d-'a') + 10
		case d >= 'A' && d <= 'F':
			nib = int(d-'A') + 10
		default:
			return 0, jsonEscapeFormatError()
		}
		v = (v << 4) | nib
	}
	p.pos += 4
	return v, nil
}

// parseNumber parses a JSON number token (RFC 8259 grammar) into an exact Decimal. No leading zeros
// (`01` is malformed), a `.` requires fractional digits, `e`/`E` an exponent. The value is built via
// the shared decimal-from-parts path so a jsonb number reads identically to a numeric literal (`1e2`
// → `100`, `1.50` keeps scale 2). An out-of-cap magnitude → 22003.
func (p *jsonParser) parseNumber() (JsonNode, error) {
	start := p.pos
	neg := false
	if c, ok := p.peek(); ok && c == '-' {
		p.pos++
		neg = true
	}
	// Integer part: `0` alone, or a nonzero digit followed by more digits.
	c, ok := p.peek()
	switch {
	case ok && c == '0':
		p.pos++
	case ok && c >= '1' && c <= '9':
		for {
			d, ok := p.peek()
			if !ok || d < '0' || d > '9' {
				break
			}
			p.pos++
		}
	default:
		return JsonNode{}, jsonMalformed("invalid number")
	}
	intEnd := p.pos
	negLen := 0
	if neg {
		negLen = 1
	}
	intPart := string(p.buf[start+negLen : intEnd])

	// Fractional part.
	frac := ""
	if c, ok := p.peek(); ok && c == '.' {
		p.pos++
		fs := p.pos
		for {
			d, ok := p.peek()
			if !ok || d < '0' || d > '9' {
				break
			}
			p.pos++
		}
		if p.pos == fs {
			return JsonNode{}, jsonMalformed("expected digits after decimal point")
		}
		frac = string(p.buf[fs:p.pos])
	}

	// Exponent.
	hasExp := false
	var exp int64
	if c, ok := p.peek(); ok && (c == 'e' || c == 'E') {
		p.pos++
		var esign int64 = 1
		if c2, ok := p.peek(); ok && c2 == '-' {
			p.pos++
			esign = -1
		} else if ok && c2 == '+' {
			p.pos++
		}
		es := p.pos
		var mag int64
		for {
			d, ok := p.peek()
			if !ok || d < '0' || d > '9' {
				break
			}
			// Clamp to the decimal exponent limit while scanning (decimal.go expLimit); an
			// exponent this large already drives the value past the caps → 22003.
			mag = saturatingClamp(mag*10+int64(d-'0'), expLimit)
			p.pos++
		}
		if p.pos == es {
			return JsonNode{}, jsonMalformed("expected digits in exponent")
		}
		hasExp = true
		exp = esign * mag
	}

	digits, scale := decimalFromParts(intPart, frac, hasExp, exp)
	d, err := decimalFromDigitsScale(neg, digits, scale).CheckCap()
	if err != nil {
		return JsonNode{}, err
	}
	return JsonNode{Kind: JNumber, Num: d}, nil
}

// saturatingClamp clamps v to [0, limit] (the exponent magnitude scan keeps the accumulation bounded
// and inside i64; an exponent past the caps still traps 22003 via CheckCap).
func saturatingClamp(v, limit int64) int64 {
	if v < 0 || v > limit {
		return limit
	}
	return v
}

// utf8Len is the UTF-8 lead-byte length (1..4). A continuation/invalid lead byte returns 1 so the
// copy path's decode check rejects it.
func utf8Len(lead byte) int {
	switch {
	case lead < 0x80:
		return 1
	case lead>>5 == 0b110:
		return 2
	case lead>>4 == 0b1110:
		return 3
	case lead>>3 == 0b11110:
		return 4
	default:
		return 1
	}
}

// canonicalizeObject canonicalizes object members (spec/design/json.md §2.3): drop duplicate keys
// keeping the LAST occurrence (PG jsonb last-wins), then sort the survivors length-then-bytewise.
// Done before sorting so the stored object has unique keys in canonical order — a pure function of
// input.
func canonicalizeObject(members []JsonMember) []JsonMember {
	// Last-wins dedup, preserving the value of the last occurrence (re-sort follows so first-
	// appearance order is irrelevant).
	out := make([]JsonMember, 0, len(members))
	for _, m := range members {
		found := false
		for i := range out {
			if out[i].Key == m.Key {
				out[i].Val = m.Val
				found = true
				break
			}
		}
		if !found {
			out = append(out, m)
		}
	}
	// Insertion sort by canonical key order (small objects; a stable, dependency-free sort that is
	// byte-identical across cores).
	for i := 1; i < len(out); i++ {
		for j := i; j > 0 && jsonKeyCmp(out[j].Key, out[j-1].Key) < 0; j-- {
			out[j], out[j-1] = out[j-1], out[j]
		}
	}
	return out
}

// makeObject builds a canonical `jsonb` object node from (key, value) members — last-wins dedup then
// the canonical key sort (json.md §2.3). The constructor for jsonb_build_object (the Rust json::make_object).
func makeObject(members []JsonMember) JsonNode {
	return JsonNode{Kind: JObject, Obj: canonicalizeObject(members)}
}

// hasDuplicateKeys detects a duplicate object key anywhere in the tree — for `IS JSON WITH UNIQUE
// KEYS` (json-sql-functions.md §5). The node must be parsed with parsePreservingJSON (which keeps
// duplicate keys); this walks every object (and recurses into member values and array elements).
func hasDuplicateKeys(node *JsonNode) bool {
	switch node.Kind {
	case JObject:
		for i := range node.Obj {
			for j := i + 1; j < len(node.Obj); j++ {
				if node.Obj[i].Key == node.Obj[j].Key {
					return true
				}
			}
		}
		for i := range node.Obj {
			if hasDuplicateKeys(&node.Obj[i].Val) {
				return true
			}
		}
		return false
	case JArray:
		for i := range node.Arr {
			if hasDuplicateKeys(&node.Arr[i]) {
				return true
			}
		}
		return false
	default:
		return false
	}
}

// jsonPredKindMatches reports whether a parsed JSON node matches an `IS JSON [kind]` predicate's
// kind (json-sql-functions.md §5).
func jsonPredKindMatches(node *JsonNode, kind jsonPredicateKind) bool {
	switch kind {
	case jPKScalar:
		return node.Kind != JObject && node.Kind != JArray
	case jPKArray:
		return node.Kind == JArray
	case jPKObject:
		return node.Kind == JObject
	default: // JPKValue
		return true
	}
}

// ---------------------------------------------------------------------------------------------
// Output (`jsonbOut` — the canonical PG render). `json_out` is the stored verbatim text.
// ---------------------------------------------------------------------------------------------

// jsonbOut renders a jsonb node to the canonical PG text (spec/design/json.md §6.2): one space after
// each `:` and `,`, keys in canonical order, numbers via the Decimal renderer (scale preserved),
// strings JSON-escaped, `true`/`false`/`null` lowercase.
func jsonbOut(node *JsonNode) string {
	var b strings.Builder
	writeJSONNode(node, &b)
	return b.String()
}

func writeJSONNode(node *JsonNode, out *strings.Builder) {
	switch node.Kind {
	case JNull:
		out.WriteString("null")
	case JBool:
		if node.B {
			out.WriteString("true")
		} else {
			out.WriteString("false")
		}
	case JNumber:
		out.WriteString(node.Num.Render())
	case JString:
		writeJSONString(node.S, out)
	case JArray:
		out.WriteByte('[')
		for i := range node.Arr {
			if i > 0 {
				out.WriteString(", ")
			}
			writeJSONNode(&node.Arr[i], out)
		}
		out.WriteByte(']')
	default: // JObject
		out.WriteByte('{')
		for i := range node.Obj {
			if i > 0 {
				out.WriteString(", ")
			}
			writeJSONString(node.Obj[i].Key, out)
			out.WriteString(": ")
			writeJSONNode(&node.Obj[i].Val, out)
		}
		out.WriteByte('}')
	}
}

// jsonCompactOut renders a node tree to COMPACT JSON text — no space after `:` or `,` — the form
// PG's `json` processing functions (`json_strip_nulls`, `to_json`, the json builders) emit (a `json`
// value's output style, distinct from `jsonb`'s spaced canonical form). Members render in their node
// order (the caller controls canonicalization; a `json`-on-demand parse keeps input order).
func jsonCompactOut(node *JsonNode) string {
	var b strings.Builder
	writeCompactJSON(node, &b)
	return b.String()
}

func writeCompactJSON(node *JsonNode, out *strings.Builder) {
	switch node.Kind {
	case JNull:
		out.WriteString("null")
	case JBool:
		if node.B {
			out.WriteString("true")
		} else {
			out.WriteString("false")
		}
	case JNumber:
		out.WriteString(node.Num.Render())
	case JString:
		writeJSONString(node.S, out)
	case JArray:
		out.WriteByte('[')
		for i := range node.Arr {
			if i > 0 {
				out.WriteByte(',')
			}
			writeCompactJSON(&node.Arr[i], out)
		}
		out.WriteByte(']')
	default: // JObject
		out.WriteByte('{')
		for i := range node.Obj {
			if i > 0 {
				out.WriteByte(',')
			}
			writeJSONString(node.Obj[i].Key, out)
			out.WriteByte(':')
			writeCompactJSON(&node.Obj[i].Val, out)
		}
		out.WriteByte('}')
	}
}

// writeJSONString JSON-escapes a string the way PG escape_json does: quote, escape `"` and `\`, the
// short escapes for `\b \f \n \r \t`, other control chars (< 0x20) as `\u00XX`; `/` is NOT escaped
// and non-ASCII is emitted as raw UTF-8. Iterates by code point (the escape decision is per-rune)
// while sorting/comparison stays bytewise.
func writeJSONString(s string, out *strings.Builder) {
	out.WriteByte('"')
	for _, ch := range s {
		switch ch {
		case '"':
			out.WriteString("\\\"")
		case '\\':
			out.WriteString("\\\\")
		case '\b':
			out.WriteString("\\b")
		case '\f':
			out.WriteString("\\f")
		case '\n':
			out.WriteString("\\n")
		case '\r':
			out.WriteString("\\r")
		case '\t':
			out.WriteString("\\t")
		default:
			if ch < 0x20 {
				out.WriteString("\\u")
				h := strconv.FormatInt(int64(ch), 16)
				for k := len(h); k < 4; k++ {
					out.WriteByte('0')
				}
				out.WriteString(h)
			} else {
				out.WriteRune(ch)
			}
		}
	}
	out.WriteByte('"')
}

// ---------------------------------------------------------------------------------------------
// Accessor operators (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1) — jsonb kernels over
// the canonical node tree. (The `json` overloads, which return verbatim sub-text, are the
// "Verbatim `json` navigation" section below — json.md §4.)
// ---------------------------------------------------------------------------------------------

// jsonGetField is `jsonb -> text`: an object field by key. nil (→ SQL NULL) if the node is not an
// object or the key is absent. A duplicate-key object cannot occur (jsonb is canonical, unique keys).
func jsonGetField(node *JsonNode, key string) *JsonNode {
	if node.Kind != JObject {
		return nil
	}
	for i := range node.Obj {
		if node.Obj[i].Key == key {
			return &node.Obj[i].Val
		}
	}
	return nil
}

// jsonGetIndex is `jsonb -> int`: an array element by index (a negative index counts from the end).
// nil (→ SQL NULL) if the node is not an array or the index is out of range.
func jsonGetIndex(node *JsonNode, idx int64) *JsonNode {
	if node.Kind != JArray {
		return nil
	}
	length := int64(len(node.Arr))
	i := idx
	if i < 0 {
		i = length + i
	}
	if i >= 0 && i < length {
		return &node.Arr[i]
	}
	return nil
}

// jsonGetPath is `jsonb #> text[]`: navigate a path of text steps. At each step an object uses the
// step as a key; an array parses the step as an integer index (a non-integer or out-of-range step →
// nil). An empty path returns the whole node (PG). nil (→ SQL NULL) if any step fails.
func jsonGetPath(node *JsonNode, path []string) *JsonNode {
	cur := node
	for _, step := range path {
		switch cur.Kind {
		case JObject:
			next := jsonGetField(cur, step)
			if next == nil {
				return nil
			}
			cur = next
		case JArray:
			idx, ok := jsonPathStepIndex(step)
			if !ok {
				return nil
			}
			next := jsonGetIndex(cur, idx)
			if next == nil {
				return nil
			}
			cur = next
		default:
			return nil
		}
	}
	return cur
}

// jsonNodeToText is the `->>` / `#>>` text rendering of an accessed node: a STRING node yields its
// raw content (unescaped); a JSON `null` node yields SQL NULL (ok=false); every other node yields its
// canonical jsonb_out text.
func jsonNodeToText(node *JsonNode) (string, bool) {
	switch node.Kind {
	case JNull:
		return "", false
	case JString:
		return node.S, true
	default:
		return jsonbOut(node), true
	}
}

// jsonPathStepIndex parses a `#>` / `#>>` path step as an array index the way PostgreSQL does
// (`strtoint(step, 10)` in get_path_all / get_jsonb_path_all): optional leading C-locale whitespace,
// an optional sign, at least one digit, nothing trailing, and the value within i32. Anything else is
// ok=false — a step that never matches an array element (PG's INT_MIN sentinel; '1 ' and an
// out-of-i32 step miss).
func jsonPathStepIndex(step string) (int64, bool) {
	s := strings.TrimLeft(step, " \t\n\v\f\r")
	digits := s
	if len(digits) > 0 && (digits[0] == '+' || digits[0] == '-') {
		digits = digits[1:]
	}
	if digits == "" {
		return 0, false
	}
	for i := 0; i < len(digits); i++ {
		if digits[i] < '0' || digits[i] > '9' {
			return 0, false
		}
	}
	v, err := strconv.ParseInt(s, 10, 64)
	if err != nil || v < math.MinInt32 || v > math.MaxInt32 {
		return 0, false
	}
	return v, true
}

// ---------------------------------------------------------------------------------------------
// Verbatim `json` navigation (spec/design/json.md §4, json-sql-functions.md §1/§3). A `json` value
// is its validated input text; its accessors and SRFs return SUB-TEXT spans of that text exactly as
// written (whitespace, key order, duplicate keys kept), mirroring PostgreSQL's streaming get_worker /
// each_worker / elements_worker (jsonfuncs.c). The text is already well-formed (json_in validated
// it), so the scanners below only skip, never re-validate.
// ---------------------------------------------------------------------------------------------

// verbatimStep is one step of a verbatim-json access path (PG's tpath[i] / ipath[i] pair): hasKey/key
// matches an object member by its de-escaped name; hasIndex/index an array element.
type verbatimStep struct {
	key      string
	hasKey   bool
	index    int64
	hasIndex bool
}

func isJSONWS(b byte) bool { return b == ' ' || b == '\t' || b == '\n' || b == '\r' }

func skipJSONWS(b string, pos int) int {
	for pos < len(b) && isJSONWS(b[pos]) {
		pos++
	}
	return pos
}

// skipJSONString returns the end (exclusive) of the string token whose opening `"` is at pos.
func skipJSONString(b string, pos int) int {
	pos++
	for pos < len(b) {
		switch b[pos] {
		case '"':
			return pos + 1
		case '\\':
			pos += 2
		default:
			pos++
		}
	}
	return pos
}

// skipJSONValue returns the end (exclusive) of the well-formed value starting at pos (no leading
// whitespace).
func skipJSONValue(b string, pos int) int {
	switch b[pos] {
	case '"':
		return skipJSONString(b, pos)
	case '{', '[':
		depth := 0
		p := pos
		for p < len(b) {
			switch b[p] {
			case '"':
				p = skipJSONString(b, p)
				continue
			case '{', '[':
				depth++
			case '}', ']':
				depth--
				if depth == 0 {
					return p + 1
				}
			}
			p++
		}
		return p
	default:
		// A number / true / false / null: runs to the next delimiter.
		p := pos
		for p < len(b) && !isJSONWS(b[p]) && b[p] != ',' && b[p] != ']' && b[p] != '}' {
			p++
		}
		return p
	}
}

// decodeJSONString decodes the string token at pos (its opening `"`) to its content, reusing the
// parser's escape decoding. Returns (content, end).
func decodeJSONString(b string, pos int) (string, int, error) {
	end := skipJSONString(b, pos)
	p := &jsonParser{buf: []byte(b[pos:end]), needEscapes: true}
	s, err := p.parseString()
	if err != nil {
		return "", 0, err
	}
	return s, end, nil
}

// forEachJSONMember visits the members of the object whose `{` is at pos: f(key, valueStart) must
// return the value's end. Returns the end of the object.
func forEachJSONMember(b string, pos int, f func(key string, start int) (int, error)) (int, error) {
	p := skipJSONWS(b, pos+1)
	if b[p] == '}' {
		return p + 1, nil
	}
	for {
		key, kend, err := decodeJSONString(b, p)
		if err != nil {
			return 0, err
		}
		p = skipJSONWS(b, kend) + 1 // past ':'
		p = skipJSONWS(b, p)
		end, err := f(key, p)
		if err != nil {
			return 0, err
		}
		p = skipJSONWS(b, end)
		if b[p] == ',' {
			p = skipJSONWS(b, p+1)
		} else {
			return p + 1, nil // past '}'
		}
	}
}

// forEachJSONElement visits the elements of the array whose `[` is at pos: f(ordinal, valueStart)
// must return the element's end. Returns the end of the array.
func forEachJSONElement(b string, pos int, f func(ord int64, start int) (int, error)) (int, error) {
	p := skipJSONWS(b, pos+1)
	if b[p] == ']' {
		return p + 1, nil
	}
	var i int64
	for {
		end, err := f(i, p)
		if err != nil {
			return 0, err
		}
		p = skipJSONWS(b, end)
		i++
		if b[p] == ',' {
			p = skipJSONWS(b, p+1)
		} else {
			return p + 1, nil // past ']'
		}
	}
}

// verbatimGet is the verbatim `json` access `-> ->> #> #>>` with a NON-empty path (PG get_worker):
// the sub-text of the LAST value, in document order, whose path matches steps — an object step
// matches EVERY member of that name (duplicate keys included, so a later duplicate overrides an
// earlier match), an array step the element at its index. A negative index is resolved PG's way: on
// entering ANY array at that depth (matched or not), if the index is negative and in range it is
// rebased onto that array's length, and the rebased index then carries to every later array at the
// same depth (an observable PG quirk, pinned by the oracle). ok=false → SQL NULL.
func verbatimGet(text string, steps []verbatimStep) (string, bool, error) {
	w := &verbatimWalk{b: text, steps: steps, index: make([]int64, len(steps)), hasIndex: make([]bool, len(steps)), resStart: -1}
	for i, s := range steps {
		w.index[i], w.hasIndex[i] = s.index, s.hasIndex
	}
	if _, err := w.walk(skipJSONWS(text, 0), 0, true); err != nil {
		return "", false, err
	}
	if w.resStart < 0 {
		return "", false, nil
	}
	return text[w.resStart:w.resEnd], true, nil
}

type verbatimWalk struct {
	b     string
	steps []verbatimStep
	// index / hasIndex: the per-depth array index, rebased in place on a negative subscript.
	index            []int64
	hasIndex         []bool
	resStart, resEnd int
}

// walk walks the value at pos (nested in depth containers); pathOK = the path matched down to here.
// Returns the value's end.
func (w *verbatimWalk) walk(pos, depth int, pathOK bool) (int, error) {
	n := len(w.steps)
	if depth >= n {
		return skipJSONValue(w.b, pos), nil
	}
	switch w.b[pos] {
	case '{':
		return forEachJSONMember(w.b, pos, func(key string, vstart int) (int, error) {
			matched := pathOK && w.steps[depth].hasKey && w.steps[depth].key == key
			vend, err := w.walk(vstart, depth+1, matched && depth+1 < n)
			if err != nil {
				return 0, err
			}
			if matched && depth+1 == n {
				w.resStart, w.resEnd = vstart, vend
			}
			return vend, nil
		})
	case '[':
		if w.hasIndex[depth] && w.index[depth] < 0 {
			var count int64
			if _, err := forEachJSONElement(w.b, pos, func(_ int64, s int) (int, error) {
				count++
				return skipJSONValue(w.b, s), nil
			}); err != nil {
				return 0, err
			}
			if i := w.index[depth]; i != math.MinInt64 && -i <= count {
				w.index[depth] = i + count
			}
		}
		return forEachJSONElement(w.b, pos, func(ord int64, vstart int) (int, error) {
			matched := pathOK && w.hasIndex[depth] && w.index[depth] == ord
			vend, err := w.walk(vstart, depth+1, matched && depth+1 < n)
			if err != nil {
				return 0, err
			}
			if matched && depth+1 == n {
				w.resStart, w.resEnd = vstart, vend
			}
			return vend, nil
		})
	default:
		return skipJSONValue(w.b, pos), nil
	}
}

// verbatimWhole is the verbatim `json #> '{}'` / `#>> '{}'` (an EMPTY path) span — the whole
// document as PG's get_worker returns it: an object/array without its surrounding whitespace, but a
// SCALAR from the very start of the text (keeping any leading whitespace) to the end of its token.
func verbatimWhole(text string) string {
	start := skipJSONWS(text, 0)
	end := skipJSONValue(text, start)
	if text[start] == '{' || text[start] == '[' {
		return text[start:end]
	}
	return text[:end]
}

// verbatimToText is the `->>` / `#>>` / `_text` render of a verbatim sub-text: a JSON string → its
// de-escaped content, a JSON null → SQL NULL (ok=false), anything else → the sub-text verbatim.
func verbatimToText(sub string) (string, bool, error) {
	start := skipJSONWS(sub, 0)
	switch sub[start] {
	case '"':
		s, _, err := decodeJSONString(sub, start)
		if err != nil {
			return "", false, err
		}
		return s, true, nil
	case 'n':
		return "", false, nil
	default:
		return sub, true, nil
	}
}

// verbatimArrayElements is json_array_elements[_text](json) (PG elements_worker): the verbatim
// sub-text of each top-level array element, in order. A non-array → 22023 with PG's message. The
// _text form (deEscape) lexes with need_escapes, so a `\u0000` / unpaired surrogate raises — but an
// object root's error comes first (PG raises it at the `{`), while a scalar root is lexed before its
// own error (json.md §4.2).
func verbatimArrayElements(text, fn string, deEscape bool) ([]string, error) {
	start := skipJSONWS(text, 0)
	if text[start] == '{' {
		return nil, newError(InvalidParameterValue, fmt.Sprintf("cannot call %s on a non-array", fn))
	}
	if deEscape {
		if err := checkJSONEscapes(text); err != nil {
			return nil, err
		}
	}
	if text[start] != '[' {
		return nil, newError(InvalidParameterValue, fmt.Sprintf("cannot call %s on a scalar", fn))
	}
	var out []string
	_, err := forEachJSONElement(text, start, func(_ int64, s int) (int, error) {
		e := skipJSONValue(text, s)
		out = append(out, text[s:e])
		return e, nil
	})
	return out, err
}

// verbatimMember is one json_each member: its de-escaped key and verbatim value sub-text.
type verbatimMember struct {
	key, value string
}

// verbatimObjectMembers is json_each[_text](json) (PG each_worker): each top-level object member as
// its de-escaped key and verbatim value sub-text, in INPUT order with duplicate keys kept. A
// non-object → 22023 with PG's message. Both forms lex with need_escapes, so a `\u0000` / unpaired
// surrogate raises — after an array root's error (PG raises it at the `[`), before a scalar root's
// (json.md §4.2).
func verbatimObjectMembers(text string) ([]verbatimMember, error) {
	start := skipJSONWS(text, 0)
	if text[start] == '[' {
		return nil, newError(InvalidParameterValue, "cannot deconstruct an array as an object")
	}
	if err := checkJSONEscapes(text); err != nil {
		return nil, err
	}
	if text[start] != '{' {
		return nil, newError(InvalidParameterValue, "cannot deconstruct a scalar")
	}
	var out []verbatimMember
	_, err := forEachJSONMember(text, start, func(key string, s int) (int, error) {
		e := skipJSONValue(text, s)
		out = append(out, verbatimMember{key: key, value: text[s:e]})
		return e, nil
	})
	return out, err
}

// ---------------------------------------------------------------------------------------------
// Containment / existence operators (`@> <@ ? ?| ?&`, spec/design/json-sql-functions.md §1, J5).
// ---------------------------------------------------------------------------------------------

// jsonContains is `a @> b` — does the jsonb document `a` deeply contain `b` (PG `jsonb_contains` /
// `JsonbDeepContains`, spec/design/json-sql-functions.md §1.2)? The rules, pinned against the
// postgres:18 oracle:
//   - top level: object @> object; array @> array; array @> a bare SCALAR that equals one of its
//     direct elements (PostgreSQL's raw-scalar pseudo-array); scalar @> an equal scalar. Every other
//     pairing (object vs non-object, scalar vs array) is false.
//   - below the top level a contained value must be the SAME kind (jsonContainsSameKind): the
//     bare-scalar-in-array rule is top-level only, so `{"x":["a"]} @> {"x":"a"}` is false.
func jsonContains(a, b *JsonNode) bool {
	if a.Kind == JArray && !jsonIsContainer(b) {
		// array @> a bare scalar (top level only): the scalar is a direct element of the array.
		for i := range a.Arr {
			if jsonbValueEqual(&a.Arr[i], b) {
				return true
			}
		}
		return false
	}
	return jsonContainsSameKind(a, b)
}

// jsonContainsSameKind is containment of two values of the same kind (PG `JsonbDeepContains` below the
// top level): object ⊇ object (every member of `b` has a member of `a` with the same key whose value
// contains it, same kind), array ⊇ array (every element of `b` is matched by an element of `a`), or two
// equal scalars. Any kind mismatch is false.
func jsonContainsSameKind(a, b *JsonNode) bool {
	switch {
	case a.Kind == JObject && b.Kind == JObject:
		for i := range b.Obj {
			va := jsonGetField(a, b.Obj[i].Key)
			if va == nil || !jsonContainsSameKind(va, &b.Obj[i].Val) {
				return false
			}
		}
		return true
	case a.Kind == JArray && b.Kind == JArray:
		for i := range b.Arr {
			if !jsonElementInArray(a.Arr, &b.Arr[i]) {
				return false
			}
		}
		return true
	case !jsonIsContainer(a) && !jsonIsContainer(b):
		// scalar = scalar: value equality (numbers by value, strings by bytes).
		return jsonbValueEqual(a, b)
	default:
		return false
	}
}

// jsonElementInArray reports whether `e` (an element of the right array) is "contained in" the left
// array `arr`: a scalar element must EQUAL a direct element of `arr`; an object/array element must be
// contained in some same-kind direct element of `arr`.
func jsonElementInArray(arr []JsonNode, e *JsonNode) bool {
	switch e.Kind {
	case JObject:
		for i := range arr {
			if arr[i].Kind == JObject && jsonContainsSameKind(&arr[i], e) {
				return true
			}
		}
		return false
	case JArray:
		for i := range arr {
			if arr[i].Kind == JArray && jsonContainsSameKind(&arr[i], e) {
				return true
			}
		}
		return false
	default: // scalar
		for i := range arr {
			if jsonbValueEqual(&arr[i], e) {
				return true
			}
		}
		return false
	}
}

// jsonIsContainer reports whether a node is a container (object or array) vs a scalar
// (null/bool/number/string).
func jsonIsContainer(n *JsonNode) bool {
	return n.Kind == JObject || n.Kind == JArray
}

// jsonHasKey is `jsonb ? text` — does the document have this top-level key? An object: the key is
// present; an array: the key is a string element; a string scalar: it equals the key; otherwise
// false (PG semantics, oracle-pinned).
func jsonHasKey(node *JsonNode, key string) bool {
	switch node.Kind {
	case JObject:
		for i := range node.Obj {
			if node.Obj[i].Key == key {
				return true
			}
		}
		return false
	case JArray:
		for i := range node.Arr {
			if node.Arr[i].Kind == JString && node.Arr[i].S == key {
				return true
			}
		}
		return false
	case JString:
		return node.S == key
	default:
		return false
	}
}

// ---------------------------------------------------------------------------------------------
// Mutation operators (`|| - #-`, spec/design/json-sql-functions.md §1, J6).
// ---------------------------------------------------------------------------------------------

// badNullValueTreatment is jsonb_set_lax's 22023 for a NULL or unrecognized null_value_treatment
// (PostgreSQL's exact text).
func badNullValueTreatment() *EngineError {
	return newError(InvalidParameterValue, `null_value_treatment must be "delete_key", "return_target", "use_json_null", or "raise_exception"`)
}

// nullValueRaised is jsonb_set_lax's 22004 for a SQL NULL value under null_value_treatment =>
// 'raise_exception', with PostgreSQL's Detail and Hint (error-fields.md §9.2, §11.4).
func nullValueRaised() *EngineError {
	return newError(NullValueNotAllowed, "JSON value must not be null").
		withDetail(`Exception was raised because null_value_treatment is "raise_exception".`).
		withHint("To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed.")
}

// cannotDelete builds the 22023 (invalid_parameter_value) error for an illegal delete target.
func cannotDelete(msg string) *EngineError {
	return newError(InvalidParameterValue, msg)
}

// jsonConcat is `a || b` — concatenate / shallow-merge (PG): two objects merge with the RIGHT side
// winning on a key conflict (result re-canonicalized); otherwise each operand is treated as an array
// (an array stays, a non-array becomes a one-element array) and the two are concatenated.
func jsonConcat(a, b *JsonNode) JsonNode {
	if a.Kind == JObject && b.Kind == JObject {
		out := make([]JsonMember, len(a.Obj))
		copy(out, a.Obj)
		for _, m := range b.Obj {
			found := false
			for i := range out {
				if out[i].Key == m.Key {
					out[i].Val = m.Val
					found = true
					break
				}
			}
			if !found {
				out = append(out, m)
			}
		}
		// Insertion sort by canonical key order (small objects; byte-identical across cores).
		for i := 1; i < len(out); i++ {
			for j := i; j > 0 && jsonKeyCmp(out[j].Key, out[j-1].Key) < 0; j-- {
				out[j], out[j-1] = out[j-1], out[j]
			}
		}
		return JsonNode{Kind: JObject, Obj: out}
	}
	elems := jsonToArrayElems(a)
	elems = append(elems, jsonToArrayElems(b)...)
	return JsonNode{Kind: JArray, Arr: elems}
}

// jsonToArrayElems treats a node as an array for `||`: an array contributes its elements, any other
// node becomes a single one-element list.
func jsonToArrayElems(n *JsonNode) []JsonNode {
	if n.Kind == JArray {
		out := make([]JsonNode, len(n.Arr))
		copy(out, n.Arr)
		return out
	}
	return []JsonNode{*n}
}

// jsonDeleteKey is `jsonb - text` — delete a key from an object, or delete every matching string
// element from an array; a scalar is `22023` ("cannot delete from scalar").
func jsonDeleteKey(node *JsonNode, key string) (JsonNode, error) {
	switch node.Kind {
	case JObject:
		out := make([]JsonMember, 0, len(node.Obj))
		for _, m := range node.Obj {
			if m.Key != key {
				out = append(out, m)
			}
		}
		return JsonNode{Kind: JObject, Obj: out}, nil
	case JArray:
		out := make([]JsonNode, 0, len(node.Arr))
		for i := range node.Arr {
			if node.Arr[i].Kind == JString && node.Arr[i].S == key {
				continue
			}
			out = append(out, node.Arr[i])
		}
		return JsonNode{Kind: JArray, Arr: out}, nil
	default:
		return JsonNode{}, cannotDelete("cannot delete from scalar")
	}
}

// jsonDeleteIndex is `jsonb - int` — delete the array element at an index (negative from the end;
// out of range is a no-op). An object is `22023` ("cannot delete from object using integer index");
// a scalar is `22023` ("cannot delete from scalar").
func jsonDeleteIndex(node *JsonNode, idx int64) (JsonNode, error) {
	switch node.Kind {
	case JArray:
		length := int64(len(node.Arr))
		i := idx
		if i < 0 {
			i = length + i
		}
		if i < 0 || i >= length {
			return *node, nil
		}
		out := make([]JsonNode, 0, len(node.Arr)-1)
		out = append(out, node.Arr[:i]...)
		out = append(out, node.Arr[i+1:]...)
		return JsonNode{Kind: JArray, Arr: out}, nil
	case JObject:
		return JsonNode{}, cannotDelete("cannot delete from object using integer index")
	default:
		return JsonNode{}, cannotDelete("cannot delete from scalar")
	}
}

// jsonRootCount is the element count of a container root (`JB_ROOT_COUNT`); a scalar counts as one.
func jsonRootCount(node *JsonNode) int {
	switch node.Kind {
	case JObject:
		return len(node.Obj)
	case JArray:
		return len(node.Arr)
	default:
		return 1
	}
}

// jsonTextPathArg extracts a jsonb mutator's text[] path / key-list argument, keeping NULL elements as
// nil (each kernel decides what one means: jsonb_set / jsonb_insert / #- raise 22004 when the walk
// reaches it, `- text[]` skips it). A multidimensional array is 2202E ("wrong number of array
// subscripts"), checked before anything else about the target (PG). The resolver guarantees a
// non-NULL text[] value.
func jsonTextPathArg(v Value) ([]*string, error) {
	arr := v.arrayVal()
	if arr.Ndim() > 1 {
		return nil, newError(ArraySubscriptError, "wrong number of array subscripts")
	}
	path := make([]*string, len(arr.Elements))
	for i, el := range arr.Elements {
		if el.Kind == ValText {
			s := el.str()
			path[i] = &s
		}
	}
	return path, nil
}

// jsonDeleteKeys is `jsonb - text[]` (PG `jsonb_delete_array`) — delete every key (object) / matching
// string element (array) named by a non-NULL element of `keys`; a NULL (nil) element is skipped. A
// scalar root is 22023 ("cannot delete from scalar") even for empty `keys`; an empty container is
// returned unchanged.
func jsonDeleteKeys(node *JsonNode, keys []*string) (JsonNode, error) {
	if !jsonIsContainer(node) {
		return JsonNode{}, cannotDelete("cannot delete from scalar")
	}
	cur := *node
	for _, k := range keys {
		if k == nil {
			continue
		}
		next, err := jsonDeleteKey(&cur, *k)
		if err != nil {
			return JsonNode{}, err
		}
		cur = next
	}
	return cur, nil
}

// pathOp is the walk a path mutation performs — PostgreSQL's setPath op_type bits, restricted to the
// SQL functions (the subscript-assignment FILL_GAPS / CONSISTENT_POSITION bits never arise here).
type pathOp int

const (
	poReplace      pathOp = iota // jsonb_set(..., create_if_missing => false)
	poCreate                     // jsonb_set(..., create_if_missing => true)
	poInsertBefore               // jsonb_insert(..., insert_after => false)
	poInsertAfter                // jsonb_insert(..., insert_after => true)
	poDelete                     // #- / jsonb_set_lax(..., 'delete_key')
)

// addsMissing is JB_PATH_CREATE_OR_INSERT: whether a missing final key / index is added.
func (op pathOp) addsMissing() bool {
	return op == poCreate || op == poInsertBefore || op == poInsertAfter
}

// pathSetMode selects whether a path mutation REPLACES at the final step (`jsonb_set`) or INSERTS a
// new element (`jsonb_insert`). For Insert, the flag is `insert_after` (place after the array index,
// not before); for Set, the flag is `create_if_missing` (add a missing final key / out-of-range
// index).
type pathSetMode int

const (
	psSet pathSetMode = iota
	psInsert
)

// jsonDeletePath is `jsonb #- text[]` (PG `jsonb_delete_path`) — delete the element at a path. A
// scalar root is 22023 ("cannot delete path in scalar"), even for an empty path; an empty container
// or an empty path returns the target unchanged; otherwise the setPathWalk rules apply (a NULL path
// element reached → 22004, a non-integer step into an array → 22P02, a missing / out-of-range /
// scalar step → no-op).
func jsonDeletePath(node *JsonNode, path []*string) (JsonNode, error) {
	if !jsonIsContainer(node) {
		return JsonNode{}, cannotDelete("cannot delete path in scalar")
	}
	if jsonRootCount(node) == 0 || len(path) == 0 {
		return *node, nil
	}
	return setPathWalk(node, path, 0, &JsonNode{Kind: JNull}, poDelete)
}

// setPath is `jsonb_set(target, path, value[, create_if_missing])` (json-sql-functions.md §2, PG
// `jsonb_set`): set the value at `path` (a text[] of object keys / array indices). A scalar root is
// 22023 ("cannot set path in scalar"), even for an empty path; an empty container without `create`,
// or an empty path, returns the target unchanged. At the final step an existing element is REPLACED
// and a missing one is added only when `create` (an out-of-range index appends; one before the start
// prepends). See setPathWalk for the per-step rules.
func setPath(node *JsonNode, path []*string, value *JsonNode, create bool) (JsonNode, error) {
	if !jsonIsContainer(node) {
		return JsonNode{}, cannotDelete("cannot set path in scalar")
	}
	if (jsonRootCount(node) == 0 && !create) || len(path) == 0 {
		return *node, nil
	}
	op := poReplace
	if create {
		op = poCreate
	}
	return setPathWalk(node, path, 0, value, op)
}

// insertPath is `jsonb_insert(target, path, value[, insert_after])` (json-sql-functions.md §2, PG
// `jsonb_insert`): like setPath but the final step INSERTS — an existing object key is 22023 ("cannot
// replace existing key"); an array index inserts before it (or after, when `insert_after`). A scalar
// root is 22023 ("cannot set path in scalar"); an empty path returns the target unchanged (an empty
// container does NOT short-circuit).
func insertPath(node *JsonNode, path []*string, value *JsonNode, insertAfter bool) (JsonNode, error) {
	if !jsonIsContainer(node) {
		return JsonNode{}, cannotDelete("cannot set path in scalar")
	}
	if len(path) == 0 {
		return *node, nil
	}
	op := poInsertBefore
	if insertAfter {
		op = poInsertAfter
	}
	return setPathWalk(node, path, 0, value, op)
}

// pathStepInt32 is PostgreSQL's `strtoint(step, &end, 10)` acceptance for an array path step:
// optional leading C whitespace, an optional sign, then one or more ASCII digits to the end, within
// int32. ok is false for anything else (no digits, trailing characters, or out of int32 range).
func pathStepInt32(step string) (int32, bool) {
	s := strings.TrimLeft(step, " \t\n\v\f\r")
	neg := false
	if s != "" && (s[0] == '-' || s[0] == '+') {
		neg = s[0] == '-'
		s = s[1:]
	}
	if s == "" {
		return 0, false
	}
	var mag int64
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c < '0' || c > '9' {
			return 0, false
		}
		mag = mag*10 + int64(c-'0')
		if mag > 1<<31 {
			return 0, false // beyond every int32 magnitude
		}
	}
	v := mag
	if neg {
		v = -mag
	}
	if v < math.MinInt32 || v > math.MaxInt32 {
		return 0, false
	}
	return int32(v), true
}

// setPathWalk is the recursive step of a path mutation (PG setPath / setPathObject / setPathArray), at
// path position `level` (< len(path)). A NULL step reached is 22004 ("path element at position N is
// null", 1-based) — checked before the node is inspected, so a NULL step under a scalar still raises,
// while one past a missing key / out-of-range index is never reached. A scalar node below the root is
// returned unchanged. Object: the key matching the step is replaced / deleted at the final step (an
// insert op on an existing key is 22023 "cannot replace existing key") or descended into; a missing
// final key is added by the adding ops. Array: the step must be an int32 (else 22P02 "path element at
// position N is not an integer"); a negative index counts from the end, one before the start becomes
// a prepend for the adding ops, one past the end clamps to the length (an append); a non-final
// missing index is a no-op.
func setPathWalk(node *JsonNode, path []*string, level int, value *JsonNode, op pathOp) (JsonNode, error) {
	if path[level] == nil {
		return JsonNode{}, newError(NullValueNotAllowed,
			"path element at position "+strconv.Itoa(level+1)+" is null")
	}
	step := *path[level]
	isFinal := level+1 == len(path)
	switch node.Kind {
	case JObject:
		out := make([]JsonMember, 0, len(node.Obj)+1)
		done := false
		for _, mem := range node.Obj {
			if !done && mem.Key == step {
				done = true
				if isFinal {
					if op == poInsertBefore || op == poInsertAfter {
						return JsonNode{}, cannotDelete("cannot replace existing key").
							withHint("Try using the function jsonb_set to replace key value.")
					}
					if op != poDelete {
						out = append(out, JsonMember{Key: mem.Key, Val: *value})
					}
				} else {
					child, err := setPathWalk(&mem.Val, path, level+1, value, op)
					if err != nil {
						return JsonNode{}, err
					}
					out = append(out, JsonMember{Key: mem.Key, Val: child})
				}
			} else {
				out = append(out, mem)
			}
		}
		if !done && isFinal && op.addsMissing() {
			// A missing final key is added, sorted into canonical place.
			out = canonicalizeObject(append(out, JsonMember{Key: step, Val: *value}))
		}
		return JsonNode{Kind: JObject, Obj: out}, nil
	case JArray:
		parsed, ok := pathStepInt32(step)
		if !ok {
			return JsonNode{}, newError(InvalidTextRepresentation,
				"path element at position "+strconv.Itoa(level+1)+" is not an integer: \""+step+"\"")
		}
		nelems := int64(len(node.Arr))
		// before is PG's INT_MIN sentinel: a negative index before the start (matches nothing; an
		// adding op prepends).
		idx := int64(parsed)
		before := false
		if parsed < 0 {
			if -int64(parsed) > nelems {
				before = true
			} else {
				idx = nelems + int64(parsed)
			}
		}
		if !before && idx > nelems {
			idx = nelems
		}
		out := make([]JsonNode, 0, len(node.Arr)+1)
		done := false
		if (before || nelems == 0) && isFinal && op.addsMissing() {
			out = append(out, *value)
			done = true
		}
		for i := range node.Arr {
			el := &node.Arr[i]
			if !before && idx == int64(i) {
				done = true
				if isFinal {
					if op == poInsertBefore || op == poCreate {
						out = append(out, *value)
					}
					if op == poInsertBefore || op == poInsertAfter {
						out = append(out, *el)
					}
					if op == poInsertAfter || op == poReplace {
						out = append(out, *value)
					}
				} else {
					child, err := setPathWalk(el, path, level+1, value, op)
					if err != nil {
						return JsonNode{}, err
					}
					out = append(out, child)
				}
			} else {
				out = append(out, *el)
			}
		}
		if !done && isFinal && op.addsMissing() {
			out = append(out, *value)
		}
		return JsonNode{Kind: JArray, Arr: out}, nil
	default:
		// A scalar below the root: the path cannot continue, so the value stays (PG).
		return *node, nil
	}
}

// ---------------------------------------------------------------------------------------------
// Processing / introspection functions (B1, spec/design/json-sql-functions.md §2).
// ---------------------------------------------------------------------------------------------

// jsonTypeofName is `json[b]_typeof` — the JSON type name of a node (PG): `object`/`array`/`string`/
// `number`/`boolean`/`null`.
func jsonTypeofName(node *JsonNode) string {
	switch node.Kind {
	case JNull:
		return "null"
	case JBool:
		return "boolean"
	case JNumber:
		return "number"
	case JString:
		return "string"
	case JArray:
		return "array"
	default: // JObject
		return "object"
	}
}

// jsonArrayLength is `json[b]_array_length` — the element count of an array node; a non-array is
// `22023`.
func jsonArrayLength(node *JsonNode) (int64, error) {
	if node.Kind != JArray {
		return 0, newError(InvalidParameterValue, "cannot get array length of a scalar")
	}
	return int64(len(node.Arr)), nil
}

// jsonStripNulls is `json[b]_strip_nulls` — recursively remove object members whose value is JSON
// `null` (array nulls are kept, PG). Objects re-canonicalize (the surviving members stay in canonical
// order; the input is already canonical for jsonb, and for json the on-demand parse order is kept).
func jsonStripNulls(node *JsonNode) JsonNode {
	switch node.Kind {
	case JObject:
		out := make([]JsonMember, 0, len(node.Obj))
		for i := range node.Obj {
			if node.Obj[i].Val.Kind == JNull {
				continue
			}
			out = append(out, JsonMember{Key: node.Obj[i].Key, Val: jsonStripNulls(&node.Obj[i].Val)})
		}
		return JsonNode{Kind: JObject, Obj: out}
	case JArray:
		out := make([]JsonNode, len(node.Arr))
		for i := range node.Arr {
			out[i] = jsonStripNulls(&node.Arr[i])
		}
		return JsonNode{Kind: JArray, Arr: out}
	default:
		return *node
	}
}

// jsonPretty is `jsonb_pretty` — an indented multi-line render (PG: 4-space indent, one space after
// `:`). A container ALWAYS multi-lines (even an empty one: `{` newline, then the close at the
// container's own indent → `{\n}` / `{\n    }`); scalars render inline.
func jsonPretty(node *JsonNode) string {
	var b strings.Builder
	writePrettyJSON(node, 0, &b)
	return b.String()
}

// jsonPrettySize is the exact UTF-8 byte length of jsonPretty's rendering, computed without building
// it: the jsonb_pretty amplifier sizes its output before allocating (spec/design/cost.md §8.1).
// Mirrors writePrettyJSON: brackets, a newline plus four spaces per level before each member and the
// close, commas, and ": " plus the escaped key per object member. Each scalar and key is rendered on
// its own to measure it; that scratch is bounded by the input.
func jsonPrettySize(node *JsonNode) int64 {
	return jsonPrettySizeAt(node, 0)
}

func jsonPrettySizeAt(node *JsonNode, indent int64) int64 {
	// The open bracket, then the close on its own line at this container's indent.
	shell := 1 + 1 + 4*indent + 1
	// Before each member: a comma after the first, then a newline and the member's indent.
	lead := func(i int) int64 {
		n := 1 + 4*(indent+1)
		if i > 0 {
			n++
		}
		return n
	}
	switch node.Kind {
	case JObject:
		size := shell
		for i := range node.Obj {
			var key strings.Builder
			writeJSONString(node.Obj[i].Key, &key)
			size += lead(i) + int64(key.Len()) + 2 + jsonPrettySizeAt(&node.Obj[i].Val, indent+1)
		}
		return size
	case JArray:
		size := shell
		for i := range node.Arr {
			size += lead(i) + jsonPrettySizeAt(&node.Arr[i], indent+1)
		}
		return size
	default:
		var b strings.Builder
		writeJSONNode(node, &b)
		return int64(b.Len())
	}
}

func writePrettyJSON(node *JsonNode, indent int, out *strings.Builder) {
	switch node.Kind {
	case JObject:
		out.WriteByte('{')
		for i := range node.Obj {
			if i > 0 {
				out.WriteByte(',')
			}
			out.WriteByte('\n')
			pushJSONIndent(indent+1, out)
			writeJSONString(node.Obj[i].Key, out)
			out.WriteString(": ")
			writePrettyJSON(&node.Obj[i].Val, indent+1, out)
		}
		out.WriteByte('\n')
		pushJSONIndent(indent, out)
		out.WriteByte('}')
	case JArray:
		out.WriteByte('[')
		for i := range node.Arr {
			if i > 0 {
				out.WriteByte(',')
			}
			out.WriteByte('\n')
			pushJSONIndent(indent+1, out)
			writePrettyJSON(&node.Arr[i], indent+1, out)
		}
		out.WriteByte('\n')
		pushJSONIndent(indent, out)
		out.WriteByte(']')
	default:
		writeJSONNode(node, out)
	}
}

func pushJSONIndent(level int, out *strings.Builder) {
	for i := 0; i < level; i++ {
		out.WriteString("    ")
	}
}
