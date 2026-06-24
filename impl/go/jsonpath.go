package jed

import (
	"strconv"
	"strings"
	"unicode/utf8"
)

// The jsonpath type's compiler + canonical renderer (spec/design/jsonpath.md, slice P1a).
//
// P1a is the LITERAL-ONLY surface (like J0 for json): the jsonpath scalar type, the
// '…'::jsonpath / jsonpath '…' literal cast (compiled at resolve), and the canonical render
// ($.a → $."a", lax omitted, strict kept). The structural-accessor subset is parsed here
// ($, .key, .*, [subscripts], [*], numeric / last indices, to slices, lax/strict mode);
// the eval engine, filters, item methods, arithmetic, like_regex, and $name variables are a
// deferred P1b follow-on (a valid-PG path using one → 0A000 at compile). A malformed path is
// 42601. The compiled program is a pure function of the source — kept byte-identical cross-core
// by the conformance suite (CLAUDE.md §5: a hand-written parser, never codegenned).

// JsonPath is a compiled jsonpath (the structural-accessor subset, P1a).
type JsonPath struct {
	Strict bool
	Steps  []jpStep
}

// jpStepKind tags an accessor step.
type jpStepKind int

const (
	// jpMember is `.key` — a member accessor (the key, unescaped).
	jpMember jpStepKind = iota
	// jpWildcardMember is `.*` — the wildcard member accessor.
	jpWildcardMember
	// jpSubscripts is `[s, …]` — one or more subscripts.
	jpSubscripts
	// jpWildcardElement is `[*]` — the wildcard element accessor.
	jpWildcardElement
)

// jpStep is one accessor step.
type jpStep struct {
	kind jpStepKind
	// key holds the member name when kind == jpMember.
	key string
	// subs holds the subscript list when kind == jpSubscripts.
	subs []jpSubscript
}

// jpSubscript is one subscript: a single index or an `i to j` slice. slice == false ⇒ a single
// index (only a is meaningful); slice == true ⇒ the `a to b` form.
type jpSubscript struct {
	a, b  jpIndex
	slice bool
}

// jpIndex is a subscript index: a non-negative integer literal or the `last` sentinel.
type jpIndex struct {
	// last is true for the `last` sentinel; otherwise number is the integer value.
	last   bool
	number int64
}

// jpUnsupported is a jsonpath construct that is valid in PostgreSQL but not yet supported by jed (a
// deferred P1b follow-on): 0A000, a documented divergence.
func jpUnsupported(what string) *EngineError {
	return NewError(FeatureNotSupported, "jsonpath "+what+" is not supported yet")
}

// jpMalformed is a malformed jsonpath literal: 42601 (PostgreSQL's syntax-error class for a bad
// path literal).
func jpMalformed(detail string) *EngineError {
	return NewError(SyntaxError, "invalid jsonpath: "+detail)
}

// Compile compiles a jsonpath source string (P1a structural subset). Malformed → 42601; a valid-PG
// but unsupported construct → 0A000.
func Compile(src string) (JsonPath, error) {
	p := &jpParser{s: []byte(src)}
	p.skipWs()
	// Optional mode word: strict / lax (default lax).
	strict := false
	if p.eatKeyword("strict") {
		strict = true
	} else {
		p.eatKeyword("lax")
	}
	p.skipWs()
	if !p.eat('$') {
		// @, a variable, or a bare literal as a top-level path expression — the filter / scalar
		// path-expression surface (a P1b follow-on).
		return JsonPath{}, jpUnsupported("expressions other than a `$`-rooted accessor path")
	}
	// $name — a path variable (the $ immediately followed by a name char / quote) is a P1b
	// follow-on (the bound-variable vars surface).
	if c, ok := p.peek(); ok && (isMemberStart(c) || c == '"') {
		return JsonPath{}, jpUnsupported("path variables `$name`")
	}
	var steps []jpStep
	for {
		p.skipWs()
		c, ok := p.peek()
		if !ok {
			break
		}
		switch c {
		case '.':
			p.i++
			if p.eat('*') {
				steps = append(steps, jpStep{kind: jpWildcardMember})
			} else if nc, ok := p.peek(); ok && (nc == '"' || isMemberStart(nc)) {
				m, err := p.parseMember()
				if err != nil {
					return JsonPath{}, err
				}
				// `.identifier(` is an item-method call (a P1b follow-on); a bare identifier
				// is a member accessor.
				if pc, ok := p.peek(); ok && pc == '(' {
					return JsonPath{}, jpUnsupported("item methods")
				}
				steps = append(steps, jpStep{kind: jpMember, key: m})
			} else {
				// `$.` with nothing (or a non-member) after it is malformed.
				return JsonPath{}, jpMalformed("expected a member name after `.`")
			}
		case '[':
			p.i++
			p.skipWs()
			if p.eat('*') {
				p.skipWs()
				if !p.eat(']') {
					return JsonPath{}, jpMalformed("expected `]` after `[*`")
				}
				steps = append(steps, jpStep{kind: jpWildcardElement})
			} else {
				subs, err := p.parseSubscripts()
				if err != nil {
					return JsonPath{}, err
				}
				steps = append(steps, jpStep{kind: jpSubscripts, subs: subs})
			}
		case '?':
			return JsonPath{}, jpUnsupported("filter expressions `?(…)`")
		case '+', '-', '*', '/', '%', '=', '<', '>', '!', '&', '|':
			// Arithmetic / comparison operators on a path expression are a P1b follow-on.
			return JsonPath{}, jpUnsupported("path arithmetic / predicate operators")
		default:
			return JsonPath{}, jpMalformed("unexpected character in path")
		}
	}
	// `$` alone is valid (the root document) — steps is empty in that case.
	return JsonPath{Strict: strict, Steps: steps}, nil
}

// Render is the canonical render (spec/design/jsonpath.md §2): strict kept / lax omitted; member
// keys quoted; [*], [i], [i to j] subscripts; matches PostgreSQL's jsonpath_out.
func (jp JsonPath) Render() string {
	var out strings.Builder
	if jp.Strict {
		out.WriteString("strict ")
	}
	out.WriteByte('$')
	for _, step := range jp.Steps {
		switch step.kind {
		case jpMember:
			out.WriteByte('.')
			writeQuoted(step.key, &out)
		case jpWildcardMember:
			out.WriteString(".*")
		case jpWildcardElement:
			out.WriteString("[*]")
		case jpSubscripts:
			out.WriteByte('[')
			for n, s := range step.subs {
				if n > 0 {
					out.WriteByte(',')
				}
				if s.slice {
					writeIndex(s.a, &out)
					out.WriteString(" to ")
					writeIndex(s.b, &out)
				} else {
					writeIndex(s.a, &out)
				}
			}
			out.WriteByte(']')
		}
	}
	return out.String()
}

func writeIndex(i jpIndex, out *strings.Builder) {
	if i.last {
		out.WriteString("last")
	} else {
		out.WriteString(strconv.FormatInt(i.number, 10))
	}
}

// writeQuoted renders a member key as a canonical jsonpath quoted string ("…" with JSON escaping).
func writeQuoted(k string, out *strings.Builder) {
	out.WriteByte('"')
	for _, c := range k {
		switch c {
		case '"':
			out.WriteString("\\\"")
		case '\\':
			out.WriteString("\\\\")
		case '\n':
			out.WriteString("\\n")
		case '\r':
			out.WriteString("\\r")
		case '\t':
			out.WriteString("\\t")
		default:
			if c < 0x20 {
				out.WriteString("\\u")
				h := strconv.FormatInt(int64(c), 16)
				for len(h) < 4 {
					h = "0" + h
				}
				out.WriteString(h)
			} else {
				out.WriteRune(c)
			}
		}
	}
	out.WriteByte('"')
}

func isMemberStart(c byte) bool {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c == '_'
}

func isMemberCont(c byte) bool {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
		c == '_' || c == '$'
}

type jpParser struct {
	s []byte
	i int
}

func (p *jpParser) peek() (byte, bool) {
	if p.i < len(p.s) {
		return p.s[p.i], true
	}
	return 0, false
}

func (p *jpParser) eat(c byte) bool {
	if pc, ok := p.peek(); ok && pc == c {
		p.i++
		return true
	}
	return false
}

func (p *jpParser) skipWs() {
	for p.i < len(p.s) && isAsciiWhitespace(p.s[p.i]) {
		p.i++
	}
}

func isAsciiWhitespace(c byte) bool {
	return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v'
}

// eatKeyword consumes kw if it appears as a whole WORD at the cursor — i.e. the following byte is
// not an identifier-continuation character (so `last]`, `to `, `strict $` all match, but `lastfoo`
// does not).
func (p *jpParser) eatKeyword(kw string) bool {
	kb := []byte(kw)
	if p.i+len(kb) <= len(p.s) && string(p.s[p.i:p.i+len(kb)]) == kw {
		after := p.i + len(kb)
		if after >= len(p.s) {
			p.i += len(kb)
			return true
		}
		c := p.s[after]
		if !((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_') {
			p.i += len(kb)
			return true
		}
	}
	return false
}

// parseMember parses a member key after `.`: a bare identifier or a "…" quoted string.
func (p *jpParser) parseMember() (string, error) {
	if c, ok := p.peek(); ok && c == '"' {
		return p.parseQuoted()
	}
	start := p.i
	for {
		c, ok := p.peek()
		if !ok || !isMemberCont(c) {
			break
		}
		p.i++
	}
	if p.i == start {
		return "", jpMalformed("empty member name")
	}
	return string(p.s[start:p.i]), nil
}

// parseQuoted parses a "…" jsonpath string (JSON escapes).
func (p *jpParser) parseQuoted() (string, error) {
	p.i++ // opening "
	var out strings.Builder
	for {
		c, ok := p.peek()
		if !ok {
			return "", jpMalformed("unterminated string")
		}
		switch c {
		case '"':
			p.i++
			return out.String(), nil
		case '\\':
			p.i++
			ec, ok := p.peek()
			if !ok {
				return "", jpMalformed("invalid escape")
			}
			switch ec {
			case '"':
				out.WriteByte('"')
			case '\\':
				out.WriteByte('\\')
			case '/':
				out.WriteByte('/')
			case 'n':
				out.WriteByte('\n')
			case 'r':
				out.WriteByte('\r')
			case 't':
				out.WriteByte('\t')
			case 'b':
				out.WriteByte('\b')
			case 'f':
				out.WriteByte('\f')
			case 'u':
				if p.i+5 > len(p.s) {
					return "", jpMalformed("invalid \\u escape")
				}
				hex := string(p.s[p.i+1 : p.i+5])
				cp, err := strconv.ParseUint(hex, 16, 32)
				if err != nil {
					return "", jpMalformed("invalid \\u escape")
				}
				r := rune(cp)
				if !utf8.ValidRune(r) {
					return "", jpMalformed("invalid \\u escape")
				}
				out.WriteRune(r)
				p.i += 4
			default:
				return "", jpMalformed("invalid escape")
			}
			p.i++
		default:
			// Copy one UTF-8 char.
			start := p.i
			n := utf8Len(p.s[p.i])
			if p.i+n > len(p.s) {
				n = len(p.s) - p.i
			}
			p.i += n
			out.Write(p.s[start:p.i])
		}
	}
}

// parseSubscripts parses a [...] subscript list (the opening `[` consumed, not the wildcard form).
// Each subscript is `index` or `index to index`; `index` is a number or `last`. Anything else →
// 0A000.
func (p *jpParser) parseSubscripts() ([]jpSubscript, error) {
	var subs []jpSubscript
	for {
		p.skipWs()
		a, err := p.parseIndex()
		if err != nil {
			return nil, err
		}
		p.skipWs()
		var sub jpSubscript
		if p.eatKeyword("to") {
			p.skipWs()
			b, err := p.parseIndex()
			if err != nil {
				return nil, err
			}
			p.skipWs()
			sub = jpSubscript{a: a, b: b, slice: true}
		} else {
			sub = jpSubscript{a: a}
		}
		subs = append(subs, sub)
		c, ok := p.peek()
		switch {
		case ok && c == ',':
			p.i++
			continue
		case ok && c == ']':
			p.i++
			return subs, nil
		default:
			return nil, jpMalformed("expected `,` or `]` in subscript")
		}
	}
}

func (p *jpParser) parseIndex() (jpIndex, error) {
	if p.eatKeyword("last") {
		return jpIndex{last: true}, nil
	}
	c, ok := p.peek()
	switch {
	case !ok:
		// A truncated path (no index where one is required) is malformed.
		return jpIndex{}, jpMalformed("expected a subscript index")
	case !((c >= '0' && c <= '9') || c == '-'):
		// A non-numeric token starts an expression subscript ($.a, arithmetic) — a P1b follow-on.
		return jpIndex{}, jpUnsupported("non-literal subscript expressions")
	}
	start := p.i
	if pc, ok := p.peek(); ok && pc == '-' {
		p.i++
	}
	for {
		dc, ok := p.peek()
		if !ok || dc < '0' || dc > '9' {
			break
		}
		p.i++
	}
	if p.i == start+1 && p.s[start] == '-' {
		return jpIndex{}, jpMalformed("expected digits after `-`")
	}
	n, err := strconv.ParseInt(string(p.s[start:p.i]), 10, 64)
	if err != nil {
		return jpIndex{}, jpMalformed("subscript out of range")
	}
	return jpIndex{number: n}, nil
}
