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

func jsonMalformed(detail string) error {
	return NewError(InvalidTextRepresentation, "invalid input syntax for type json: "+detail)
}

// jsonbIn parses + canonicalizes JSON text into a jsonb node tree (`jsonb_in` — spec/design/json.md
// §6.2): numbers → Decimal, object keys deduped last-wins then sorted length-then-bytewise.
// Malformed input → 22P02.
func jsonbIn(input string) (JsonNode, error) {
	p := &jsonParser{buf: []byte(input), canonicalize: true}
	return p.parseDocument()
}

// validateJSON validates JSON text well-formedness (`json_in` — spec/design/json.md §4); the caller
// stores the original bytes verbatim. Malformed input → 22P02.
func validateJSON(input string) error {
	p := &jsonParser{buf: []byte(input), canonicalize: false}
	_, err := p.parseDocument()
	return err
}

// parsePreservingJSON parses JSON text into a node tree WITHOUT canonicalizing (object key order +
// duplicates preserved) — the on-demand structural parse a `json` operator needs
// (spec/design/json.md §4).
func parsePreservingJSON(input string) (JsonNode, error) {
	p := &jsonParser{buf: []byte(input), canonicalize: false}
	return p.parseDocument()
}

type jsonParser struct {
	buf []byte
	pos int
	// canonicalize: when true (jsonb), objects dedup last-wins and sort keys; when false (json
	// validation / on-demand parse), members are kept in input order with duplicates.
	canonicalize bool
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
		return p.parseObject()
	case c == '[':
		return p.parseArray()
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

// parseString parses a JSON string token (the leading `"` is at p.pos), decoding escapes to the
// actual UTF-8 content. RFC 8259: `\" \\ \/ \b \f \n \r \t` and `\uXXXX` (with surrogate pairs).
// Unescaped control characters (< 0x20) are rejected.
func (p *jsonParser) parseString() (string, error) {
	p.pos++ // consume opening '"'
	var out strings.Builder
	for {
		c, ok := p.peek()
		if !ok {
			return "", jsonMalformed("unterminated string")
		}
		switch {
		case c == '"':
			p.pos++
			return out.String(), nil
		case c == '\\':
			p.pos++
			e, ok := p.peek()
			if !ok {
				return "", jsonMalformed("unterminated escape")
			}
			switch e {
			case '"':
				out.WriteByte('"')
			case '\\':
				out.WriteByte('\\')
			case '/':
				out.WriteByte('/')
			case 'b':
				out.WriteByte('\b')
			case 'f':
				out.WriteByte('\f')
			case 'n':
				out.WriteByte('\n')
			case 'r':
				out.WriteByte('\r')
			case 't':
				out.WriteByte('\t')
			case 'u':
				p.pos++
				cp, err := p.parseHex4()
				if err != nil {
					return "", err
				}
				// Surrogate pair handling (UTF-16 escapes).
				if cp >= 0xD800 && cp <= 0xDBFF {
					// High surrogate: must be followed by \uDC00..\uDFFF.
					if c2, ok := p.peek(); !ok || c2 != '\\' {
						return "", jsonMalformed("unpaired high surrogate in \\u escape")
					}
					p.pos++
					if c2, ok := p.peek(); !ok || c2 != 'u' {
						return "", jsonMalformed("unpaired high surrogate in \\u escape")
					}
					p.pos++
					lo, err := p.parseHex4()
					if err != nil {
						return "", err
					}
					if lo < 0xDC00 || lo > 0xDFFF {
						return "", jsonMalformed("invalid low surrogate in \\u escape")
					}
					combined := 0x10000 + (((cp - 0xD800) << 10) | (lo - 0xDC00))
					if combined > utf8.MaxRune {
						return "", jsonMalformed("invalid surrogate pair")
					}
					out.WriteRune(rune(combined))
				} else if cp >= 0xDC00 && cp <= 0xDFFF {
					return "", jsonMalformed("unpaired low surrogate in \\u escape")
				} else {
					if cp > utf8.MaxRune {
						return "", jsonMalformed("invalid \\u escape")
					}
					out.WriteRune(rune(cp))
				}
				continue // parseHex4 already advanced pos past the 4 digits
			default:
				return "", jsonMalformed("invalid escape sequence")
			}
			p.pos++
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
			out.Write(p.buf[p.pos:end])
			p.pos = end
		}
	}
}

// parseHex4 reads exactly four hex digits as a code-unit (the cursor is just past `\u`).
func (p *jsonParser) parseHex4() (int, error) {
	if p.pos+4 > len(p.buf) {
		return 0, jsonMalformed("truncated \\u escape")
	}
	v := 0
	for i := 0; i < 4; i++ {
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
			return 0, jsonMalformed("invalid hex digit in \\u escape")
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
	d, err := DecimalFromDigitsScale(neg, digits, scale).CheckCap()
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
